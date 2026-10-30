//! iDRAC 8 web API: login, logout and the HTML5 console URL.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::blocking::{Client, RequestBuilder};
use url::Url;

pub const SESSION_COOKIE: &str = "-http-session-";

/// An authenticated iDRAC web session.
#[derive(Debug, Clone)]
pub struct Session {
    pub host: String,
    pub user: String,
    pub cookie: String,
    pub st1: String,
    pub st2: String,
    /// Per-login AIM session id, used by the viewer to request KVM credentials.
    pub aim_session: String,
    pub vm_priv: bool,
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

pub fn login(client: &Client, host: &str, user: &str, password: &str) -> Result<Session> {
    let resp = client
        .post(format!("https://{host}/data/login"))
        .form(&[("user", user), ("password", password)])
        .send()
        .with_context(|| format!("connecting to https://{host}"))?;

    let cookie = resp
        .cookies()
        .find(|c| c.name() == SESSION_COOKIE)
        .map(|c| c.value().to_string());
    let body = resp.text().context("reading login reply")?;
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
        st1,
        st2,
        aim_session: String::new(),
        vm_priv: false,
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
    session.aim_session = vars["aimSession"]
        .as_str()
        .context("no aimSession in session variables")?
        .to_string();
    session.vm_priv = vars["accessVirtualMedia"].as_str() == Some("1");
    Ok(session)
}

fn authed_get(client: &Client, session: &Session, path: &str) -> RequestBuilder {
    client
        .get(format!("https://{}/{path}", session.host))
        .header("Cookie", format!("{SESSION_COOKIE}={}", session.cookie))
        .header("ST2", &session.st2)
}

/// The UI string table the viewer expects to find on `window.opener.top.localeObj`.
pub fn locale(client: &Client, session: &Session, lang: &str) -> Result<serde_json::Value> {
    let mut table: serde_json::Value =
        authed_get(client, session, &format!("locale/locale_{lang}.json"))
            .send()
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.json())
            .context("downloading viewer locale")?;
    Ok(table["localeString"].take())
}

pub fn logout(client: &Client, session: &Session) -> Result<()> {
    authed_get(client, session, "data/logout")
        .timeout(Duration::from_secs(5))
        .send()
        .context("logging out")?;
    Ok(())
}

/// What Redfish says about the server behind the iDRAC.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Identity {
    pub service_tag: String,
    pub model: String,
}

impl Identity {
    /// "idrac-<tag>, <model>", as the web UI titles the console.
    pub fn title(&self) -> Option<String> {
        match (self.service_tag.is_empty(), self.model.is_empty()) {
            (false, false) => Some(format!("idrac-{}, {}", self.service_tag, self.model)),
            (false, true) => Some(format!("idrac-{}", self.service_tag)),
            (true, false) => Some(self.model.clone()),
            (true, true) => None,
        }
    }
}

/// Best-effort service tag and model from Redfish; `None` if Redfish is unavailable.
pub fn identity(client: &Client, host: &str, user: &str, password: &str) -> Option<Identity> {
    let sys: serde_json::Value = client
        .get(format!(
            "https://{host}/redfish/v1/Systems/System.Embedded.1"
        ))
        .basic_auth(user, Some(password))
        .timeout(Duration::from_secs(10))
        .send()
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .ok()?;
    let identity = Identity {
        service_tag: sys["SKU"].as_str().unwrap_or_default().to_string(),
        model: sys["Model"].as_str().unwrap_or_default().to_string(),
    };
    identity.title().map(|_| identity)
}

/// The URL the iDRAC web UI opens when "Launch Virtual Console" is clicked.
///
/// The viewer slices parameters out of the raw URL by position and decodes them with
/// `decodeURIComponent`, so the order is fixed and spaces must be `%20`, not `+`.
pub fn console_url(session: &Session, title: &str, lang: &str) -> Url {
    let enc = |s: &str| utf8_percent_encode(s, NON_ALPHANUMERIC).to_string();
    let url = format!(
        "https://{host}/virtualconsolehtml5.html?ipAddr={ip}&kvmPort=5900&vmPriv={vm}\
         &title={title}&lang={lang}&aimSession={aim}&ST2={st2}&TokenName=ST1&TokenKey={st1}",
        host = session.host,
        ip = enc(&session.host),
        vm = session.vm_priv,
        title = enc(&format!("{title}, User: {}", session.user)),
        lang = enc(lang),
        aim = enc(&session.aim_session),
        st2 = session.st2,
        st1 = session.st1,
    );
    Url::parse(&url).expect("valid console URL")
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
            st1: "0123456789abcdef0123456789abcdef".into(),
            st2: "fedcba9876543210fedcba9876543210".into(),
            aim_session: "1".into(),
            vm_priv: true,
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
}
