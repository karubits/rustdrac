//! iDRAC web API: login, logout and the HTML5 console page, for iDRAC 8 and iDRAC 9.
//!
//! The two generations have different web UIs: iDRAC 8 logs in with `POST /data/login`
//! (ST1/ST2 tokens) and runs the viewer as a popup of its main window; iDRAC 9 logs in with
//! `POST /sysmgmt/2015/bmc/session` (an XSRF token) and hands the viewer its KVM session ids
//! through `sessionStorage`.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::blocking::{Client, RequestBuilder};
use url::Url;

use crate::bmc::{ConsolePage, Cookie};

const SESSION_COOKIE: &str = "-http-session-";

const IDRAC9_SESSION: &str = "/sysmgmt/2015/bmc/session";
const IDRAC9_VCONSOLE: &str = "/sysmgmt/2015/server/vconsole";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Generation {
    Idrac8,
    Idrac9,
}

/// An authenticated iDRAC web session.
#[derive(Debug, Clone)]
pub struct Session {
    pub host: String,
    pub user: String,
    pub cookie: String,
    pub auth: Auth,
}

#[derive(Debug, Clone)]
pub enum Auth {
    Idrac8 {
        st1: String,
        st2: String,
        /// Per-login AIM session id, used by the viewer to request KVM credentials.
        aim_session: String,
        vm_priv: bool,
    },
    Idrac9 {
        xsrf: String,
    },
}

/// The iDRAC refused the request because of the name it was addressed by.
///
/// iDRAC 8 firmware 2.81 and iDRAC 9 firmware 5.10 and later validate the HTTP `Host`
/// header (`idrac.webserver.HostHeaderCheck`, on by default) as part of Dell's fix for
/// the host header injection flaw CVE-2021-21510 (DSA-2021-041). Any request whose `Host` is not
/// the iDRAC's IP address, its RAC name / DNS name or an entry of its
/// `ManualDNSEntry` allow-list gets `Access Error: 400 -- Bad Request`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostHeaderRejected {
    pub host: String,
}

impl HostHeaderRejected {
    /// The host without any port.
    fn name(&self) -> &str {
        match self.host.rsplit_once(':') {
            Some((h, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => h,
            _ => &self.host,
        }
    }

    /// An iDRAC always accepts its own IP address, so a refused IP literal means something
    /// in between rewrites the Host header, or this is not an iDRAC.
    pub fn is_ip(&self) -> bool {
        self.name().parse::<std::net::IpAddr>().is_ok()
    }

    /// Why the iDRAC answers this way.
    pub fn why(&self) -> String {
        if self.is_ip() {
            return format!(
                "An iDRAC answers this way when the HTTP Host header is not a name it knows \
                 (its Host header check, idrac.webserver.HostHeaderCheck, is on by default \
                 since iDRAC 8 firmware 2.81 and iDRAC 9 firmware 5.10), but it always \
                 accepts its own IP address. So \
                 something between here and {} rewrites the Host header (a proxy or port \
                 forward), or that address is not an iDRAC.",
                self.host
            );
        }
        format!(
            "The iDRAC rejected the name \"{}\": its HTTP Host header check is on \
             (idrac.webserver.HostHeaderCheck, the default since iDRAC 8 firmware 2.81 and \
             iDRAC 9 firmware 5.10). \
             Dell added it with the fix for CVE-2021-21510 (DSA-2021-041), a host header \
             injection flaw: a request addressed to a name the iDRAC does not know could \
             poison caches or redirect users, so it only answers to its IP addresses, its \
             RAC name and DNS name (iDRAC.NIC.DNSRacName + DNSDomainName; by default \
             idrac-<service tag>) and the names in idrac.webserver.ManualDNSEntry. The \
             TLS certificate plays no part in this. Its IP address still works.",
            self.host
        )
    }

    /// Ways to make the name work, as (what to do, racadm commands). Run them over SSH to
    /// the iDRAC's IP address (racadm is built in) or from a host with Dell's racadm.
    pub fn fixes(&self) -> Vec<(String, Vec<String>)> {
        if self.is_ip() {
            return Vec::new();
        }
        // Without any port: the iDRAC's names never carry one.
        let host = self.name();
        let (name, domain) = match host.split_once('.') {
            Some((n, d)) => (n.to_string(), d.to_string()),
            None => (host.to_string(), "example.com".to_string()),
        };
        vec![
            (
                "Give the iDRAC this name, so the FQDN matches:".to_string(),
                vec![
                    format!("racadm set iDRAC.NIC.DNSRacName {name}"),
                    format!("racadm set iDRAC.NIC.DNSDomainName {domain}"),
                ],
            ),
            (
                "Or allow it as an extra name (up to four, comma-separated; aliases, \
                 short names and NAT addresses go here too):"
                    .to_string(),
                vec![format!(
                    "racadm set idrac.webserver.ManualDNSEntry {host},{name}"
                )],
            ),
            (
                "Turning the check off is only justified for iDRAC Service Module host \
                 routes, or firmware where ManualDNSEntry is ignored (update it first):"
                    .to_string(),
                vec!["racadm set idrac.webserver.HostHeaderCheck 0".to_string()],
            ),
        ]
    }
}

impl std::fmt::Display for HostHeaderRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: Access Error: 400 -- Bad Request\n\n{}",
            self.host,
            self.why()
        )?;
        for (what, commands) in self.fixes() {
            write!(f, "\n\n{what}")?;
            for c in commands {
                write!(f, "\n    {c}")?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for HostHeaderRejected {}

/// Whether an HTTP reply is the host header check's refusal.
pub fn host_header_rejected(status: reqwest::StatusCode, body: &str) -> bool {
    status == reqwest::StatusCode::BAD_REQUEST && body.contains("Access Error: 400 -- Bad Request")
}

/// Fields of the XML document returned by `POST /data/login`.
#[derive(Debug, PartialEq)]
struct LoginReply {
    auth_result: i32,
    forward_url: String,
    blocking_time: u32,
    error_msg: String,
}

pub fn http_client() -> Result<Client> {
    Client::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(20))
        .build()
        .context("building HTTP client")
}

/// Which web UI the iDRAC runs, from where `/` redirects to: `/restgui/…` on iDRAC 9.
pub(crate) fn generation_from_path(path: &str) -> Generation {
    if path.starts_with("/restgui/") {
        Generation::Idrac9
    } else {
        Generation::Idrac8
    }
}

pub fn login(
    client: &Client,
    host: &str,
    user: &str,
    password: &str,
    generation: Generation,
) -> Result<Session> {
    match generation {
        Generation::Idrac8 => login8(client, host, user, password),
        Generation::Idrac9 => login9(client, host, user, password),
    }
}

fn login8(client: &Client, host: &str, user: &str, password: &str) -> Result<Session> {
    let resp = client
        .post(format!("https://{host}/data/login"))
        .form(&[("user", user), ("password", password)])
        .send()
        .with_context(|| format!("connecting to https://{host}"))?;

    let cookie = resp
        .cookies()
        .find(|c| c.name() == SESSION_COOKIE)
        .map(|c| c.value().to_string());
    let status = resp.status();
    let body = resp.text().context("reading login reply")?;
    if host_header_rejected(status, &body) {
        return Err(HostHeaderRejected {
            host: host.to_string(),
        }
        .into());
    }
    let reply = parse_login_reply(&body)?;

    if reply.auth_result != 0 {
        let mut msg = match reply.auth_result {
            1 => "invalid username or password".to_string(),
            n => format!("login rejected (authResult {n})"),
        };
        if !reply.error_msg.is_empty() {
            msg += &format!(": {}", reply.error_msg);
        }
        if reply.blocking_time > 0 {
            msg += &format!(" — login blocked for {}s", reply.blocking_time);
        }
        bail!("{host}: {msg}");
    }

    let (st1, st2) = parse_tokens(&reply.forward_url)
        .with_context(|| format!("no ST1/ST2 tokens in forwardUrl {:?}", reply.forward_url))?;
    let cookie = cookie.context("login succeeded but no session cookie was set")?;

    let mut session = Session {
        host: host.to_string(),
        user: user.to_string(),
        cookie,
        auth: Auth::Idrac8 {
            st1,
            st2,
            aim_session: String::new(),
            vm_priv: false,
        },
    };

    let vars: serde_json::Value = authed_get(
        client,
        &session,
        "session?getSsnVar=aimSession,accessVirtualMedia",
    )
    .send()
    .and_then(|r| r.error_for_status())
    .and_then(|r| r.json())
    .context("reading session variables")?;
    let vars = &vars["getSsnVar"];
    let aim = vars["aimSession"]
        .as_str()
        .context("no aimSession in session variables")?
        .to_string();
    let vm = vars["accessVirtualMedia"].as_str() == Some("1");
    if let Auth::Idrac8 {
        aim_session,
        vm_priv,
        ..
    } = &mut session.auth
    {
        *aim_session = aim;
        *vm_priv = vm;
    }
    Ok(session)
}

/// iDRAC 9: the web UI sends the credentials as quoted header values.
fn login9(client: &Client, host: &str, user: &str, password: &str) -> Result<Session> {
    let resp = client
        .post(format!("https://{host}{IDRAC9_SESSION}"))
        .header("user", format!("\"{user}\""))
        .header("password", format!("\"{password}\""))
        .send()
        .with_context(|| format!("connecting to https://{host}"))?;
    let xsrf = resp
        .headers()
        .get("XSRF-TOKEN")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let cookie = resp
        .cookies()
        .find(|c| c.name() == SESSION_COOKIE)
        .map(|c| c.value().to_string());
    let status = resp.status();
    let body = resp.text().context("reading login reply")?;
    if host_header_rejected(status, &body) {
        return Err(HostHeaderRejected {
            host: host.to_string(),
        }
        .into());
    }
    let reply: serde_json::Value =
        serde_json::from_str(&body).with_context(|| format!("unexpected login reply: {body}"))?;
    let auth_result = reply["authResult"].as_i64().unwrap_or(-1);
    if auth_result != 0 {
        let msg = match auth_result {
            1 => "invalid username or password".to_string(),
            5 => "too many sessions are open on the iDRAC".to_string(),
            7 | 9 => "the iDRAC requires a password change; log in once in a browser".to_string(),
            10 | 12 | 13 => "two-factor authentication is not supported yet".to_string(),
            n => format!("login rejected (authResult {n})"),
        };
        let blocked = reply["blockingTime"].as_u64().unwrap_or(0);
        let msg = if blocked > 0 {
            format!("{msg} — login blocked for {blocked}s")
        } else {
            msg
        };
        bail!("{host}: {msg}");
    }
    Ok(Session {
        host: host.to_string(),
        user: user.to_string(),
        cookie: cookie.context("login succeeded but no session cookie was set")?,
        auth: Auth::Idrac9 {
            xsrf: xsrf.context("login succeeded but no XSRF token was returned")?,
        },
    })
}

fn authed(
    client: &Client,
    method: reqwest::Method,
    session: &Session,
    path: &str,
) -> RequestBuilder {
    let req = client
        .request(
            method,
            format!("https://{}/{}", session.host, path.trim_start_matches('/')),
        )
        .header("Cookie", format!("{SESSION_COOKIE}={}", session.cookie));
    match &session.auth {
        Auth::Idrac8 { st2, .. } => req.header("ST2", st2),
        Auth::Idrac9 { xsrf } => req.header("XSRF-TOKEN", xsrf),
    }
}

fn authed_get(client: &Client, session: &Session, path: &str) -> RequestBuilder {
    authed(client, reqwest::Method::GET, session, path)
}

/// The UI string table the viewer expects to find on `window.opener.top.localeObj`.
fn locale(client: &Client, session: &Session, lang: &str) -> Result<serde_json::Value> {
    let mut table: serde_json::Value =
        authed_get(client, session, &format!("locale/locale_{lang}.json"))
            .send()
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.json())
            .context("downloading viewer locale")?;
    Ok(table["localeString"].take())
}

pub fn logout(client: &Client, session: &Session) -> Result<()> {
    let req = match session.auth {
        Auth::Idrac8 { .. } => authed_get(client, session, "data/logout"),
        Auth::Idrac9 { .. } => authed(client, reqwest::Method::DELETE, session, IDRAC9_SESSION),
    };
    req.timeout(Duration::from_secs(5))
        .send()
        .context("logging out")?;
    Ok(())
}

/// Everything a console tab needs to show the viewer for this session.
pub fn console_page(
    client: &Client,
    session: &Session,
    title: &str,
    lang: &str,
) -> Result<ConsolePage> {
    match &session.auth {
        Auth::Idrac8 { .. } => {
            let locale = locale(client, session, lang)?;
            Ok(ConsolePage {
                url: console_url(session, title, lang).to_string(),
                script: idrac8_opener_shim(&locale),
                cookies: vec![session_cookie(session)],
                disconnect_js: Some(IDRAC8_DISCONNECT_JS),
            })
        }
        Auth::Idrac9 { .. } => {
            let reply: serde_json::Value = authed_get(client, session, IDRAC9_VCONSOLE)
                .send()
                .and_then(|r| r.error_for_status())
                .and_then(|r| r.json())
                .context("asking the iDRAC to start a console session")?;
            let location = reply["Location"]
                .as_str()
                .context("the iDRAC returned no console location")?;
            let mut page = idrac9_console_page(location)?;
            page.cookies.push(session_cookie(session));
            Ok(page)
        }
    }
}

fn session_cookie(session: &Session) -> Cookie {
    Cookie {
        name: SESSION_COOKIE.to_string(),
        value: session.cookie.clone(),
        http_only: true,
    }
}

/// What the iDRAC 8 viewer's "Disconnect Viewer" button runs, minus its `window.close()`.
const IDRAC8_DISCONNECT_JS: &str =
    "try { disconnectVM(); } catch (e) {} try { exitViewer(); } catch (e) {}";

/// iDRAC 9 answers `GET …/vconsole` with the viewer URL, whose query carries the KVM session
/// ids. The web UI strips the query, puts the ids into `sessionStorage` and opens the bare
/// page as a popup (which inherits that storage); do the same with an injected script.
fn idrac9_console_page(location: &str) -> Result<ConsolePage> {
    let url = Url::parse(location).with_context(|| format!("bad console location {location:?}"))?;
    let param = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    let mut items = serde_json::Map::new();
    for key in ["ip", "kvmport", "VCSID", "VCSID2"] {
        let value = param(key).with_context(|| format!("no {key} in the console location"))?;
        items.insert(key.to_string(), value.into());
    }
    let mut page = url.clone();
    page.set_query(None);
    let path = page.path().to_string();
    let items = serde_json::Value::Object(items);
    Ok(ConsolePage {
        url: page.to_string(),
        script: format!(
            r#"(function () {{
    if (window.top !== window || location.pathname !== {path:?}) return;
    var items = {items};
    for (var k in items) sessionStorage.setItem(k, items[k]);
}})();"#
        ),
        cookies: vec![],
        disconnect_js: None,
    })
}

/// The URL the iDRAC web UI opens when "Launch Virtual Console" is clicked.
///
/// The viewer slices parameters out of the raw URL by position and decodes them with
/// `decodeURIComponent`, so the order is fixed and spaces must be `%20`, not `+`.
fn console_url(session: &Session, title: &str, lang: &str) -> Url {
    let Auth::Idrac8 {
        st1,
        st2,
        aim_session,
        vm_priv,
    } = &session.auth
    else {
        panic!("console_url is for iDRAC 8 sessions");
    };
    let enc = |s: &str| utf8_percent_encode(s, NON_ALPHANUMERIC).to_string();
    let url = format!(
        "https://{host}/virtualconsolehtml5.html?ipAddr={ip}&kvmPort=5900&vmPriv={vm}\
         &title={title}&lang={lang}&aimSession={aim}&ST2={st2}&TokenName=ST1&TokenKey={st1}",
        host = session.host,
        ip = enc(&session.host),
        vm = vm_priv,
        title = enc(&format!("{title}, User: {}", session.user)),
        lang = enc(lang),
        aim = enc(aim_session),
    );
    Url::parse(&url).expect("valid console URL")
}

/// The iDRAC 8 viewer page expects to be a popup of the iDRAC web UI and reads its string table
/// from `window.opener.top.localeObj`; without an opener its setup script throws and the
/// KVM never connects. Stand in for the web UI's main window, and for the certificate popup.
fn idrac8_opener_shim(locale: &serde_json::Value) -> String {
    format!(
        r#"(function () {{
    if (window.top !== window || window.opener) return;
    var main = {{ localeObj: {locale}, htmlViewerWindow: null, activeXWindow: null }};
    main.top = main;
    try {{
        Object.defineProperty(window, "opener", {{ value: main, configurable: true }});
    }} catch (e) {{
        window.opener = main;
    }}

    // Before connecting, the viewer opens https://<host>:5900/ in a popup so the user can
    // accept that port's certificate, and waits for the popup to postMessage back. TLS errors
    // are already ignored here, so answer on the popup's behalf.
    var realOpen = window.open;
    window.open = function (url) {{
        var target;
        try {{
            target = new URL(url, location.href);
        }} catch (e) {{
            return realOpen.apply(window, arguments);
        }}
        if (target.origin === location.origin) return realOpen.apply(window, arguments);
        var popup = {{
            closed: false,
            close: function () {{ popup.closed = true; }},
            postMessage: function () {{
                if (popup.answered) return;
                popup.answered = true;
                setTimeout(function () {{
                    window.dispatchEvent(new MessageEvent("message", {{ data: "accepted", origin: target.origin }}));
                }}, 0);
            }},
        }};
        return popup;
    }};
}})();"#
    )
}

fn xml_tag<'a>(body: &'a str, tag: &str) -> Option<&'a str> {
    let start = body.find(&format!("<{tag}>"))? + tag.len() + 2;
    let len = body[start..].find(&format!("</{tag}>"))?;
    Some(body[start..start + len].trim())
}

fn parse_login_reply(body: &str) -> Result<LoginReply> {
    let auth_result = xml_tag(body, "authResult")
        .with_context(|| format!("unexpected login reply: {body}"))?
        .parse()
        .context("parsing authResult")?;
    Ok(LoginReply {
        auth_result,
        forward_url: xml_tag(body, "forwardUrl").unwrap_or_default().to_string(),
        blocking_time: xml_tag(body, "blockingTime")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        error_msg: xml_tag(body, "errorMsg").unwrap_or_default().to_string(),
    })
}

/// Extract ST1/ST2 from `index.html?ST1=...,ST2=...`.
fn parse_tokens(forward_url: &str) -> Option<(String, String)> {
    let query = forward_url.split_once('?')?.1;
    let mut st1 = None;
    let mut st2 = None;
    for pair in query.split([',', '&']) {
        match pair.split_once('=') {
            Some(("ST1", v)) => st1 = Some(v.to_string()),
            Some(("ST2", v)) => st2 = Some(v.to_string()),
            _ => {}
        }
    }
    Some((st1?, st2?))
}

#[cfg(test)]
mod tests {
    #[test]
    fn host_header_refusal_is_recognised_and_explained() {
        use super::*;
        let body = "<html><body><h2>Access Error: 400 -- Bad Request</h2></body></html>";
        assert!(host_header_rejected(reqwest::StatusCode::BAD_REQUEST, body));
        assert!(!host_header_rejected(reqwest::StatusCode::OK, body));
        assert!(!host_header_rejected(
            reqwest::StatusCode::BAD_REQUEST,
            "nope"
        ));
        let e = HostHeaderRejected {
            host: "tat-srvpm12.oob.mujin.co.jp".into(),
        };
        let text = e.to_string();
        assert!(text.contains("CVE-2021-21510"));
        assert!(text.contains("racadm set iDRAC.NIC.DNSRacName tat-srvpm12"));
        assert!(text.contains("racadm set iDRAC.NIC.DNSDomainName oob.mujin.co.jp"));
        assert!(text.contains(
            "racadm set idrac.webserver.ManualDNSEntry tat-srvpm12.oob.mujin.co.jp,tat-srvpm12"
        ));
        assert!(text.contains("HostHeaderCheck 0"));
        let by_ip = HostHeaderRejected {
            host: "10.2.31.189:8443".into(),
        };
        assert!(by_ip.is_ip());
        assert!(by_ip.fixes().is_empty());
        assert!(by_ip.to_string().contains("proxy"));
    }

    use super::*;

    #[test]
    fn parses_successful_login() {
        let body = r#"<?xml version="1.0" encoding="UTF-8"?> <root> <status>ok</status> <authResult>0</authResult> <forwardUrl>index.html?ST1=0123456789abcdef0123456789abcdef,ST2=fedcba9876543210fedcba9876543210</forwardUrl> </root>"#;
        let reply = parse_login_reply(body).unwrap();
        assert_eq!(reply.auth_result, 0);
        let (st1, st2) = parse_tokens(&reply.forward_url).unwrap();
        assert_eq!(st1, "0123456789abcdef0123456789abcdef");
        assert_eq!(st2, "fedcba9876543210fedcba9876543210");
    }

    #[test]
    fn parses_failed_login() {
        let body = r#"<?xml version="1.0" encoding="UTF-8"?> <root> <status>ok</status> <authResult>1</authResult> <blockingTime>0</blockingTime>  <forwardUrl>index.html</forwardUrl>  <errorMsg></errorMsg></root>"#;
        let reply = parse_login_reply(body).unwrap();
        assert_eq!(
            reply,
            LoginReply {
                auth_result: 1,
                forward_url: "index.html".into(),
                blocking_time: 0,
                error_msg: String::new(),
            }
        );
        assert!(parse_tokens(&reply.forward_url).is_none());
    }

    #[test]
    fn builds_console_url_like_the_web_ui() {
        let session = Session {
            host: "192.0.2.10".into(),
            user: "admin".into(),
            cookie: String::new(),
            auth: Auth::Idrac8 {
                st1: "0123456789abcdef0123456789abcdef".into(),
                st2: "fedcba9876543210fedcba9876543210".into(),
                aim_session: "1".into(),
                vm_priv: true,
            },
        };
        let url = console_url(&session, "idrac-ABC1234, PowerEdge R630", "en");
        assert!(!url.as_str().contains('+'), "spaces must be %20: {url}");
        let pairs: Vec<(String, String)> = url.query_pairs().into_owned().collect();
        let get = |k: &str| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(url.path(), "/virtualconsolehtml5.html");
        assert_eq!(get("ipAddr"), Some("192.0.2.10"));
        assert_eq!(get("kvmPort"), Some("5900"));
        assert_eq!(get("vmPriv"), Some("true"));
        assert_eq!(
            get("title"),
            Some("idrac-ABC1234, PowerEdge R630, User: admin")
        );
        assert_eq!(get("lang"), Some("en"));
        assert_eq!(get("aimSession"), Some("1"));
        assert_eq!(get("ST2"), Some("fedcba9876543210fedcba9876543210"));
        assert_eq!(get("TokenName"), Some("ST1"));
        assert_eq!(get("TokenKey"), Some("0123456789abcdef0123456789abcdef"));
    }

    #[test]
    fn tells_generations_apart_by_redirect() {
        assert_eq!(
            generation_from_path("/restgui/start.html"),
            Generation::Idrac9
        );
        assert_eq!(generation_from_path("/start.html"), Generation::Idrac8);
        assert_eq!(generation_from_path("/"), Generation::Idrac8);
    }

    #[test]
    fn idrac9_console_page_moves_session_ids_into_storage() {
        let page = idrac9_console_page(
            "https://idrac-r6515.example.lan:443/restgui/vconsole/index.html?ip=idrac-r6515.example.lan\
             &kvmport=443&title=idrac-ABC1234,%20PowerEdge%20R6515,%20User:%20admin\
             &VCSID=0123456789abcdef&VCSID2=fedcba9876543210",
        )
        .unwrap();
        assert_eq!(
            page.url,
            "https://idrac-r6515.example.lan/restgui/vconsole/index.html"
        );
        assert!(!page.url.contains("VCSID"), "ids must not be in the URL");
        for expected in [
            r#""VCSID":"0123456789abcdef""#,
            r#""VCSID2":"fedcba9876543210""#,
            r#""ip":"idrac-r6515.example.lan""#,
            r#""kvmport":"443""#,
            r#""/restgui/vconsole/index.html""#,
        ] {
            assert!(
                page.script.contains(expected),
                "{expected} missing in {}",
                page.script
            );
        }
        assert!(page.disconnect_js.is_none());
        assert!(idrac9_console_page("https://h/restgui/vconsole/index.html?ip=h").is_err());
    }

    #[test]
    fn the_web_view_gets_the_session_cookie() {
        let session = Session {
            host: "192.0.2.10".into(),
            user: "root".into(),
            cookie: "0123456789abcdef".into(),
            auth: Auth::Idrac9 { xsrf: "x".into() },
        };
        assert_eq!(
            session_cookie(&session),
            Cookie {
                name: "-http-session-".into(),
                value: "0123456789abcdef".into(),
                http_only: true,
            }
        );
    }
}
