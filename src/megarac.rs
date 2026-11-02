//! AMI MegaRAC SP-X web UI (Gigabyte and other AMI-based BMCs): login, logout and the HTML5
//! viewer page. Health, the SEL and power go over Redfish like everywhere else.
//!
//! The web UI logs in with `POST /api/session` and gets a `QSESSIONID` cookie plus a CSRF
//! token that every later request repeats in an `X-CSRFTOKEN` header. Its HTML5 viewer
//! (`/viewer.html`) is a page the web UI opens in a popup: it expects the session details in
//! `sessionStorage` (which a popup inherits), reads its KVM token from the BMC itself and
//! opens a `wss://` socket to `/kvm`. A web view with the cookie and a script that seeds
//! `sessionStorage` is all it needs.
//!
//! Careful with the API: a request to a path the BMC does not route (it answers with an HTML
//! 403) silently ends the session.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use reqwest::blocking::Client;
use serde_json::Value;

use crate::bmc::{AuthFailed, ConsolePage, Cookie};

pub const SESSION_COOKIE: &str = "QSESSIONID";
const SESSION: &str = "/api/session";
const VIEWER: &str = "/viewer.html";
const SYSTEM_INFO: &str = "/api/host_inventory/host_interface_system_info";
const PROJECT: &str = "/api/configuration/project";

/// An authenticated web session.
#[derive(Debug, Clone)]
pub struct Session {
    pub host: String,
    pub user: String,
    pub cookie: String,
    /// Goes into `X-CSRFTOKEN` on every request.
    pub csrf: String,
    /// 1 = User, 2 = OEM, 3 = Operator, 4 = Administrator.
    pub privilege: u8,
    pub session_id: u64,
}

pub fn login(client: &Client, host: &str, user: &str, password: &str) -> Result<Session> {
    let resp = client
        .post(format!("https://{host}{SESSION}"))
        .form(&[("username", user), ("password", password)])
        .send()
        .with_context(|| format!("connecting to https://{host}"))?;
    let cookie = resp
        .cookies()
        .filter(|c| c.name() == SESSION_COOKIE && !c.value().is_empty())
        .map(|c| c.value().to_string())
        .last();
    let status = resp.status();
    let body = resp.text().context("reading login reply")?;
    parse_login(host, user, status, &body, cookie)
}

/// The login reply: `{ "ok": 0, "privilege": 4, "CSRFToken": "…", "racsession_id": 3,
/// "passwordStatus": 0, … }` on success; 401 `{ "error": "Could not login", "code": 1009 }`
/// for bad credentials.
fn parse_login(
    host: &str,
    user: &str,
    status: StatusCode,
    body: &str,
    cookie: Option<String>,
) -> Result<Session> {
    if status == StatusCode::UNAUTHORIZED {
        return Err(AuthFailed {
            host: host.to_string(),
        }
        .into());
    }
    let doc: Value = serde_json::from_str(body)
        .with_context(|| format!("{host}: login reply is not JSON (HTTP {status})"))?;
    if !status.is_success() {
        bail!("{host}: login rejected: {}", api_error(&doc, status));
    }
    if doc["passwordStatus"].as_u64() == Some(1) {
        bail!("{host}: the BMC requires a password change for {user}; do it in its web UI first");
    }
    let csrf = doc["CSRFToken"].as_str().unwrap_or_default().to_string();
    let Some(cookie) = cookie.filter(|_| !csrf.is_empty()) else {
        bail!("{host}: login succeeded but no session cookie or CSRF token was set");
    };
    Ok(Session {
        host: host.to_string(),
        user: user.to_string(),
        cookie,
        csrf,
        privilege: doc["privilege"].as_u64().unwrap_or(4).min(4) as u8,
        session_id: doc["racsession_id"].as_u64().unwrap_or(0),
    })
}

fn api_error(doc: &Value, status: StatusCode) -> String {
    match doc["error"].as_str() {
        Some(e) if !e.is_empty() => e.to_string(),
        _ => format!("HTTP {status}"),
    }
}

fn authed(
    req: reqwest::blocking::RequestBuilder,
    session: &Session,
) -> reqwest::blocking::RequestBuilder {
    req.header("Cookie", format!("{SESSION_COOKIE}={}", session.cookie))
        .header("X-CSRFTOKEN", &session.csrf)
}

pub fn logout(client: &Client, session: &Session) -> Result<()> {
    authed(
        client.delete(format!("https://{}{SESSION}", session.host)),
        session,
    )
    .timeout(Duration::from_secs(5))
    .send()
    .context("logging out")?;
    Ok(())
}

/// The system's model and serial number from the web API: this firmware's Redfish has no
/// usable serial (`SerialNumber` is null and `SKU` is the SDR version).
pub fn system_info(client: &Client, session: &Session) -> Result<(String, String)> {
    let doc: Value = authed(
        client.get(format!("https://{}{SYSTEM_INFO}", session.host)),
        session,
    )
    .timeout(Duration::from_secs(10))
    .send()
    .context("reading the system info")?
    .error_for_status()
    .context("reading the system info")?
    .json()
    .context("reading the system info")?;
    Ok(system_info_from(&doc))
}

fn system_info_from(doc: &Value) -> (String, String) {
    let sys = doc.as_array().and_then(|a| a.first()).unwrap_or(doc);
    let field = |k: &str| sys[k].as_str().unwrap_or_default().trim().to_string();
    (field("Model"), field("SerialNumber"))
}

/// The BMC's feature list (`[{"feature": "KVM_SESSION_RECONNECT"}, …]`) as the web UI
/// keeps it in `sessionStorage.features`. The viewer reads its KVM protocol options from it:
/// with `KVM_SESSION_RECONNECT` listed, the KVM service expects the reconnect-style
/// handshake and refuses the plain one ("Invalid Session Information To Reconnect").
fn project_features(client: &Client, session: &Session) -> Option<String> {
    let text = authed(
        client.get(format!("https://{}{PROJECT}", session.host)),
        session,
    )
    .timeout(Duration::from_secs(10))
    .send()
    .ok()?
    .error_for_status()
    .ok()?
    .text()
    .ok()?;
    serde_json::from_str::<Value>(&text)
        .ok()
        .filter(Value::is_array)
        .map(|v| v.to_string())
}

/// The viewer page with the session cookie and the `sessionStorage` the web UI would have
/// handed it.
pub fn console_page(client: &Client, session: &Session) -> ConsolePage {
    let features = project_features(client, session);
    ConsolePage {
        url: format!("https://{}{VIEWER}", session.host),
        script: console_shim(session, features.as_deref()),
        cookies: vec![Cookie {
            name: SESSION_COOKIE.to_string(),
            value: session.cookie.clone(),
            http_only: true,
        }],
        disconnect_js: None,
    }
}

/// What the web UI leaves in `sessionStorage` (and a readable cookie) before it opens the
/// viewer, which checks `document.cookie` first and then sends the CSRF token (`garc`) with
/// every request. Chrome goes into the user agent as the viewer prefers it. Without the
/// feature list the viewer fetches it itself.
fn console_shim(session: &Session, features: Option<&str>) -> String {
    let js = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
    let privilege =
        ["User", "OEM", "Operator", "Administrator"][session.privilege.clamp(1, 4) as usize - 1];
    format!(
        r#"(function () {{
    if (window.top !== window) return;
    try {{ document.cookie = "refresh_disable=1"; }} catch (e) {{}}
    try {{
        var ua = navigator.userAgent;
        if (ua.indexOf("Chrome") === -1) {{
            Object.defineProperty(navigator, "userAgent", {{ get: function () {{ return ua + " Chrome/120.0.0.0"; }} }});
        }}
    }} catch (e) {{}}
    var s = window.sessionStorage;
    s.setItem("garc", {csrf});
    s.setItem("username", {user});
    s.setItem("privilege", {privilege});
    s.setItem("privilege_id", {privilege_id});
    s.setItem("session_id", {session_id});
    s.setItem("id", "0");
    s.setItem("since", String(Math.floor(Date.now() / 1000)));
    s.setItem("kvm_access", "1");
    s.setItem("vmedia_access", "1");
    s.setItem("extended_privilege", "0");
    if ({features} !== null) s.setItem("features", {features});
}})();"#,
        csrf = js(&session.csrf),
        user = js(&session.user),
        privilege = js(privilege),
        privilege_id = js(&session.privilege.to_string()),
        session_id = js(&session.session_id.to_string()),
        features = features.map_or_else(|| "null".to_string(), js),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: &str = r#"{ "ok": 0, "privilege": 4, "extendedpriv": 259, "racsession_id": 3, "remote_addr": "10.2.19.15", "server_name": "10.2.31.69", "server_addr": "10.2.31.69", "HTTPSEnabled": 1, "CSRFToken": "efqP5dpz", "channel": 1, "passwordStatus": 0 }"#;

    #[test]
    fn parses_a_login() {
        let s = parse_login(
            "10.2.31.69",
            "oob-admin",
            StatusCode::OK,
            OK,
            Some("8f8278239b0932f66e9eBTLJqV2ezp".into()),
        )
        .unwrap();
        assert_eq!(s.cookie, "8f8278239b0932f66e9eBTLJqV2ezp");
        assert_eq!(s.csrf, "efqP5dpz");
        assert_eq!(s.privilege, 4);
        assert_eq!(s.session_id, 3);
        assert_eq!(s.user, "oob-admin");
    }

    #[test]
    fn tells_login_replies_apart() {
        let bad = parse_login(
            "bmc",
            "u",
            StatusCode::UNAUTHORIZED,
            r#"{ "error": "Could not login", "code": 1009 }"#,
            None,
        )
        .unwrap_err();
        assert!(crate::bmc::is_auth_failure(&bad));
        let no_cookie = parse_login("bmc", "u", StatusCode::OK, OK, None).unwrap_err();
        assert!(no_cookie.to_string().contains("no session cookie"));
        let change = parse_login(
            "bmc",
            "u",
            StatusCode::OK,
            &OK.replace("\"passwordStatus\": 0", "\"passwordStatus\": 1"),
            Some("c".into()),
        )
        .unwrap_err();
        assert!(change.to_string().contains("password change"));
        let other = parse_login(
            "bmc",
            "u",
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{ "error": "Too many sessions" }"#,
            None,
        )
        .unwrap_err();
        assert!(other.to_string().contains("Too many sessions"));
    }

    #[test]
    fn system_info_takes_the_first_system() {
        let doc: Value = serde_json::from_str(
            r#"[ { "Name": "System", "Model": "R181-NA0-00", "Manufacturer": "GIGABYTE", "SerialNumber": "DMG3ND621A0001" } ]"#,
        )
        .unwrap();
        assert_eq!(
            system_info_from(&doc),
            ("R181-NA0-00".to_string(), "DMG3ND621A0001".to_string())
        );
        assert_eq!(
            system_info_from(&Value::Null),
            (String::new(), String::new())
        );
    }

    #[test]
    fn console_page_seeds_the_viewer() {
        let session = Session {
            host: "192.0.2.20".into(),
            user: "oob-admin".into(),
            cookie: "abc".into(),
            csrf: "efqP5dpz".into(),
            privilege: 4,
            session_id: 3,
        };
        let page = ConsolePage {
            url: format!("https://{}{VIEWER}", session.host),
            script: console_shim(&session, Some(r#"[{"feature":"KVM_SESSION_RECONNECT"}]"#)),
            cookies: vec![Cookie {
                name: SESSION_COOKIE.to_string(),
                value: session.cookie.clone(),
                http_only: true,
            }],
            disconnect_js: None,
        };
        assert_eq!(page.url, "https://192.0.2.20/viewer.html");
        assert_eq!(
            page.cookies,
            vec![Cookie {
                name: "QSESSIONID".into(),
                value: "abc".into(),
                http_only: true,
            }]
        );
        assert!(page.disconnect_js.is_none());
        assert!(page.script.contains(r#"s.setItem("garc", "efqP5dpz")"#));
        assert!(
            page.script
                .contains(r#"s.setItem("privilege", "Administrator")"#)
        );
        assert!(
            page.script
                .contains(r#"s.setItem("username", "oob-admin")"#)
        );
        assert!(page.script.contains("refresh_disable=1"));
        assert!(page.script.contains("Chrome/"));
        let features = r#"[{"feature":"KVM_SESSION_RECONNECT"}]"#;
        let expected = format!(
            r#"s.setItem("features", {})"#,
            serde_json::to_string(features).unwrap()
        );
        assert!(page.script.contains(&expected), "{}", page.script);
        let bare = console_shim(&session, None);
        assert!(bare.contains("if (null !== null)"));
    }
}
