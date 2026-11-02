//! The vendor-neutral face of a BMC: which family a host belongs to, the web session that
//! runs its HTML5 console, and the server's identity from Redfish. Dell specifics live in
//! [`crate::idrac`], Supermicro specifics in [`crate::supermicro`], AMI MegaRAC (Gigabyte)
//! specifics in [`crate::megarac`].

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

use crate::idrac::{self, Generation};
use crate::megarac;
use crate::redfish;
use crate::supermicro;

pub use idrac::http_client;

/// Which BMC family. Selects the login flow, the Redfish paths and the wording; stored in
/// `connections.yaml` once learned.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Vendor {
    #[default]
    Dell,
    Supermicro,
    /// AMI MegaRAC SP-X, as on Gigabyte and ASRock Rack servers.
    MegaRac,
}

impl Vendor {
    pub const ALL: [Vendor; 3] = [Vendor::Dell, Vendor::Supermicro, Vendor::MegaRac];

    /// The YAML / combo-box id.
    pub fn id(self) -> &'static str {
        match self {
            Vendor::Dell => "dell",
            Vendor::Supermicro => "supermicro",
            Vendor::MegaRac => "megarac",
        }
    }

    pub fn parse(s: &str) -> Option<Vendor> {
        Vendor::ALL.into_iter().find(|v| v.id() == s.trim())
    }

    pub fn label(self) -> &'static str {
        match self {
            Vendor::Dell => "Dell iDRAC",
            Vendor::Supermicro => "Supermicro",
            Vendor::MegaRac => "AMI MegaRAC",
        }
    }

    /// What the vendor calls its BMC, for messages.
    pub fn bmc_name(self) -> &'static str {
        match self {
            Vendor::Dell => "iDRAC",
            Vendor::Supermicro | Vendor::MegaRac => "BMC",
        }
    }

    /// The BMC's own log, exported next to the SEL.
    pub fn aux_log_name(self) -> &'static str {
        match self {
            Vendor::Dell => "Lifecycle Controller log",
            Vendor::Supermicro => "Maintenance event log",
            Vendor::MegaRac => "Audit log",
        }
    }

    pub fn aux_log_file(self) -> &'static str {
        match self {
            Vendor::Dell => "lclog.csv",
            Vendor::Supermicro => "maintenance.csv",
            Vendor::MegaRac => "audit.csv",
        }
    }

    /// The factory administrator account.
    pub fn default_user(self) -> &'static str {
        match self {
            Vendor::Dell => "root",
            Vendor::Supermicro => "ADMIN",
            Vendor::MegaRac => "admin",
        }
    }
}

/// What probing `/` learned about a host: enough to pick the login flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Idrac(Generation),
    /// The X10/X11 web UI. Newer firmware base64-encodes the posted credentials.
    Supermicro {
        base64_login: bool,
    },
    /// The AMI MegaRAC SP-X web UI.
    MegaRac,
}

impl Platform {
    pub fn vendor(self) -> Vendor {
        match self {
            Platform::Idrac(_) => Vendor::Dell,
            Platform::Supermicro { .. } => Vendor::Supermicro,
            Platform::MegaRac => Vendor::MegaRac,
        }
    }
}

fn platforms() -> &'static Mutex<HashMap<String, Platform>> {
    static PLATFORMS: OnceLock<Mutex<HashMap<String, Platform>>> = OnceLock::new();
    PLATFORMS.get_or_init(Default::default)
}

/// Which web UI the host runs, from where `/` redirects and what it serves (no credentials
/// needed). Probed once per host per run; the alarm checker stays at one request per check.
pub fn platform(client: &Client, host: &str) -> Result<Platform> {
    let key = host.to_lowercase();
    if let Some(p) = platforms()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
    {
        return Ok(*p);
    }
    let resp = client
        .get(format!("https://{host}/"))
        .send()
        .with_context(|| format!("connecting to https://{host}"))?;
    let path = resp.url().path().to_string();
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if idrac::host_header_rejected(status, &body) {
        return Err(idrac::HostHeaderRejected {
            host: host.to_string(),
        }
        .into());
    }
    let platform = platform_from(&path, &body);
    platforms()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, platform);
    Ok(platform)
}

pub fn vendor(client: &Client, host: &str) -> Result<Vendor> {
    Ok(platform(client, host)?.vendor())
}

/// The vendor of a host probed earlier in this run, without touching the network.
pub fn cached_vendor(host: &str) -> Option<Vendor> {
    platforms()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&host.to_lowercase())
        .map(|p| p.vendor())
}

/// iDRAC 9 redirects `/` to `/restgui/…`; a Supermicro BMC serves its login form (which posts
/// to `/cgi/login.cgi`) at `/`; a MegaRAC serves its single-page app, titled "MegaRAC SP-X"
/// (ASRock Rack leaves the title empty, but the AMI logo and banner markup stay); anything
/// else is treated as an iDRAC 8, as before.
fn platform_from(final_path: &str, body: &str) -> Platform {
    if final_path.starts_with("/restgui/") {
        Platform::Idrac(Generation::Idrac9)
    } else if body.contains("/cgi/login.cgi") {
        Platform::Supermicro {
            base64_login: body.contains("btoa"),
        }
    } else if body.contains("MegaRAC") || (body.contains("ami_logo") && body.contains("h5banner")) {
        Platform::MegaRac
    } else {
        Platform::Idrac(idrac::generation_from_path(final_path))
    }
}

/// The BMC rejected the user name or password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthFailed {
    pub host: String,
}

impl std::fmt::Display for AuthFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: invalid username or password", self.host)
    }
}

impl std::error::Error for AuthFailed {}

/// Whether `e` is (or wraps) a rejected login, so the user can be asked for another password.
pub fn is_auth_failure(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<AuthFailed>().is_some())
}

/// An authenticated web session on a BMC.
#[derive(Debug, Clone)]
pub enum Session {
    Dell(idrac::Session),
    Supermicro(supermicro::Session),
    MegaRac(megarac::Session),
}

impl Session {
    pub fn vendor(&self) -> Vendor {
        match self {
            Session::Dell(_) => Vendor::Dell,
            Session::Supermicro(_) => Vendor::Supermicro,
            Session::MegaRac(_) => Vendor::MegaRac,
        }
    }

    pub fn host(&self) -> &str {
        match self {
            Session::Dell(s) => &s.host,
            Session::Supermicro(s) => &s.host,
            Session::MegaRac(s) => &s.host,
        }
    }
}

/// A cookie the console web view must carry (for the session's host, path `/`, secure).
#[derive(Debug, Clone, PartialEq)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub http_only: bool,
}

/// What a console tab loads: the viewer page, the cookies it needs, a script injected before
/// the page's own scripts run, and (iDRAC 8) the script the viewer's "Disconnect Viewer"
/// button runs.
#[derive(Debug, Clone)]
pub struct ConsolePage {
    pub url: String,
    pub script: String,
    pub cookies: Vec<Cookie>,
    pub disconnect_js: Option<&'static str>,
}

/// What Redfish says about the server behind the BMC.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Identity {
    pub vendor: Vendor,
    /// Dell's service tag, or the system serial number elsewhere.
    pub service_tag: String,
    pub model: String,
    pub power: redfish::Power,
}

impl Identity {
    /// Dell: "idrac-<tag>, <model>", as its web UI titles the console. Otherwise
    /// "<model>, S/N <serial>".
    pub fn title(&self) -> Option<String> {
        let (tag, model) = (self.service_tag.as_str(), self.model.as_str());
        match (self.vendor, tag.is_empty(), model.is_empty()) {
            (_, true, true) => None,
            (_, true, false) => Some(model.to_string()),
            (Vendor::Dell, false, true) => Some(format!("idrac-{tag}")),
            (Vendor::Dell, false, false) => Some(format!("idrac-{tag}, {model}")),
            (Vendor::Supermicro | Vendor::MegaRac, false, true) => Some(format!("S/N {tag}")),
            (Vendor::Supermicro | Vendor::MegaRac, false, false) => {
                Some(format!("{model}, S/N {tag}"))
            }
        }
    }
}

pub fn login(client: &Client, host: &str, user: &str, password: &str) -> Result<Session> {
    match platform(client, host)? {
        Platform::Idrac(generation) => {
            idrac::login(client, host, user, password, generation).map(Session::Dell)
        }
        Platform::Supermicro { base64_login } => {
            supermicro::login(client, host, user, password, base64_login).map(Session::Supermicro)
        }
        Platform::MegaRac => megarac::login(client, host, user, password).map(Session::MegaRac),
    }
}

pub fn logout(client: &Client, session: &Session) -> Result<()> {
    match session {
        Session::Dell(s) => idrac::logout(client, s),
        Session::Supermicro(s) => supermicro::logout(client, s),
        Session::MegaRac(s) => megarac::logout(client, s),
    }
}

/// Everything a console tab needs to show the viewer for this session.
pub fn console_page(
    client: &Client,
    session: &Session,
    title: &str,
    lang: &str,
) -> Result<ConsolePage> {
    match session {
        Session::Dell(s) => idrac::console_page(client, s, title, lang),
        Session::Supermicro(s) => Ok(supermicro::console_page(s)),
        Session::MegaRac(s) => Ok(megarac::console_page(client, s)),
    }
}

/// Best-effort identity from Redfish (one request, plus the reset types from `ActionInfo`
/// where the system document leaves them out); `None` if Redfish is unavailable or says
/// nothing useful. A MegaRAC's Redfish has no usable serial number, so the model and serial
/// come from its web API through the session instead.
pub fn identity(
    client: &Client,
    session: &Session,
    user: &str,
    password: &str,
) -> Option<Identity> {
    let vendor = session.vendor();
    let rf = redfish::Redfish::new(client, session.host(), user, password)
        .with_vendor(Some(vendor))
        .with_timeout(Duration::from_secs(10));
    let sys = rf.json(redfish::paths(vendor).system).ok()?;
    let mut power = redfish::power_of(&sys);
    if power.reset_types.is_empty()
        && let Some(info) = redfish::reset_action_info_link(&sys)
        && let Ok(doc) = rf.json(info)
    {
        power.reset_types = redfish::reset_types_from_action_info(&doc);
    }
    let tag = match vendor {
        Vendor::Dell => "SKU",
        Vendor::Supermicro | Vendor::MegaRac => "SerialNumber",
    };
    let mut identity = Identity {
        vendor,
        service_tag: redfish::str_field(&sys, tag),
        model: redfish::str_field(&sys, "Model"),
        power,
    };
    if let Session::MegaRac(s) = session
        && let Ok((model, serial)) = megarac::system_info(client, s)
    {
        if !model.is_empty() {
            identity.model = model;
        }
        if !serial.is_empty() {
            identity.service_tag = serial;
        }
    }
    identity.title().map(|_| identity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_platforms_apart() {
        assert_eq!(
            platform_from("/restgui/start.html", ""),
            Platform::Idrac(Generation::Idrac9)
        );
        assert_eq!(
            platform_from("/start.html", "<html>iDRAC 8</html>"),
            Platform::Idrac(Generation::Idrac8)
        );
        assert_eq!(platform_from("/", ""), Platform::Idrac(Generation::Idrac8));
        let x11 = r#"<form name="form1" action="/cgi/login.cgi" method="post">
            form1.name.value = btoa (form1.name.value);"#;
        assert_eq!(
            platform_from("/", x11),
            Platform::Supermicro { base64_login: true }
        );
        let x10 = r#"<form name="form1" action="/cgi/login.cgi" method="post">"#;
        assert_eq!(
            platform_from("/", x10),
            Platform::Supermicro {
                base64_login: false
            }
        );
        assert_eq!(
            Platform::Supermicro { base64_login: true }.vendor(),
            Vendor::Supermicro
        );
        let megarac = r#"<head>
    <meta charset="utf-8">
    <title>MegaRAC SP-X</title>
    <link rel="stylesheet" href="/styles.min.css">"#;
        assert_eq!(platform_from("/", megarac), Platform::MegaRac);
        // ASRock Rack: no title, but AMI's logo markup.
        let asrock = r#"<title></title>
    <link rel="stylesheet" href="/styles.min.css">
    <div class="hide" id="ami_logo">
        <img id="idh5banner_left"  src="images/h5banner_left.png" alt="">"#;
        assert_eq!(platform_from("/", asrock), Platform::MegaRac);
        assert_eq!(Platform::MegaRac.vendor(), Vendor::MegaRac);
    }

    #[test]
    fn vendor_ids_round_trip() {
        for v in Vendor::ALL {
            assert_eq!(Vendor::parse(v.id()), Some(v));
            assert_eq!(serde_yaml_ng::to_string(&v).unwrap().trim(), v.id());
        }
        assert_eq!(Vendor::parse("auto"), None);
    }

    #[test]
    fn titles_follow_the_vendor() {
        let dell = Identity {
            vendor: Vendor::Dell,
            service_tag: "ABC1234".into(),
            model: "PowerEdge R630".into(),
            power: Default::default(),
        };
        assert_eq!(
            dell.title().as_deref(),
            Some("idrac-ABC1234, PowerEdge R630")
        );
        let smc = Identity {
            vendor: Vendor::Supermicro,
            service_tag: "A328789X8A10489".into(),
            model: "SYS-2029BT-HNTR".into(),
            power: Default::default(),
        };
        assert_eq!(
            smc.title().as_deref(),
            Some("SYS-2029BT-HNTR, S/N A328789X8A10489")
        );
        let gbt = Identity {
            vendor: Vendor::MegaRac,
            service_tag: "DMG3ND621A0001".into(),
            model: "R181-NA0-00".into(),
            power: Default::default(),
        };
        assert_eq!(
            gbt.title().as_deref(),
            Some("R181-NA0-00, S/N DMG3ND621A0001")
        );
        assert_eq!(Identity::default().title(), None);
    }
}
