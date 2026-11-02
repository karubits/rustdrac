//! Supermicro X10/X11 BMC web UI: login, logout and the HTML5 iKVM page.
//!
//! The web UI logs in with `POST /cgi/login.cgi` (newer firmware base64-encodes the user and
//! password first) and gets an `SID` cookie. The HTML5 viewer (noVNC) is a self-contained page
//! meant to be opened in its own window: it carries a one-time KVM token and opens a
//! `wss://` socket to the BMC, so a web view with the cookie is all it needs.

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use reqwest::blocking::Client;
use reqwest::redirect::Policy;

use crate::bmc::{ConsolePage, Cookie};

pub const SID_COOKIE: &str = "SID";
const LOGIN: &str = "/cgi/login.cgi";
const LOGOUT: &str = "/cgi/logout.cgi";
const CONSOLE: &str = "/cgi/url_redirect.cgi?url_name=man_ikvm_html5_bootstrap";

/// An authenticated web session.
#[derive(Debug, Clone)]
pub struct Session {
    pub host: String,
    pub sid: String,
}

/// The shared client follows redirects, which would drop a cookie set on a 302; the login
/// request must see its own reply.
fn login_client() -> Result<Client> {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    if let Some(c) = CLIENT.get() {
        return Ok(c.clone());
    }
    let client = Client::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(20))
        .build()
        .context("building HTTP client")?;
    Ok(CLIENT.get_or_init(|| client).clone())
}

pub fn login(
    _client: &Client,
    host: &str,
    user: &str,
    password: &str,
    base64: bool,
) -> Result<Session> {
    let resp = login_client()?
        .post(format!("https://{host}{LOGIN}"))
        .form(&login_form(user, password, base64))
        .send()
        .with_context(|| format!("connecting to https://{host}"))?;
    // A failed login also sets an expired, empty SID; only a real value counts.
    let sid = resp
        .cookies()
        .filter(|c| c.name() == SID_COOKIE && !c.value().is_empty())
        .map(|c| c.value().to_string())
        .last();
    let body = resp.text().context("reading login reply")?;
    match sid {
        Some(sid) if !login_failed(&body) => Ok(Session {
            host: host.to_string(),
            sid,
        }),
        _ if body.contains("STR42_2") => Err(crate::bmc::AuthFailed {
            host: host.to_string(),
        }
        .into()),
        _ => bail!("{host}: {}", login_error(&body)),
    }
}

/// The form the login page submits (`checkform()` base64-encodes both fields on newer
/// firmware).
fn login_form(user: &str, password: &str, base64: bool) -> [(&'static str, String); 3] {
    let enc = |s: &str| {
        if base64 {
            base64::engine::general_purpose::STANDARD.encode(s)
        } else {
            s.to_string()
        }
    };
    [
        ("name", enc(user)),
        ("pwd", enc(password)),
        ("check", "00".to_string()),
    ]
}

/// A successful login answers with a script that jumps to the main menu; a failed one with
/// an alert and a refresh back to the login page.
fn login_failed(body: &str) -> bool {
    !body.contains("url_name=mainmenu")
}

fn login_error(body: &str) -> &'static str {
    if body.contains("STR42_2") {
        "invalid username or password"
    } else if body.contains("USER_LOCK") {
        "this user is locked"
    } else if body.contains("url_name=mainmenu") {
        "login succeeded but no session cookie was set"
    } else {
        "login rejected"
    }
}

pub fn logout(client: &Client, session: &Session) -> Result<()> {
    client
        .get(format!("https://{}{LOGOUT}", session.host))
        .header("Cookie", format!("{SID_COOKIE}={}", session.sid))
        .timeout(Duration::from_secs(5))
        .send()
        .context("logging out")?;
    Ok(())
}

/// The viewer page with the session cookie. Nothing else is needed: the page is what the web
/// UI itself opens in a popup, and it fetches its KVM token and port from the BMC.
pub fn console_page(session: &Session) -> ConsolePage {
    ConsolePage {
        url: format!("https://{}{CONSOLE}", session.host),
        script: console_shim().to_string(),
        cookies: vec![Cookie {
            name: SID_COOKIE.to_string(),
            value: session.sid.clone(),
            http_only: true,
        }],
        disconnect_js: None,
    }
}

/// Two stand-ins for the viewer page:
/// - It looks for "Chrome" in the user agent before it allows video recording and single-mouse
///   mode, and otherwise opens a "please use Chrome" dialog on every load. WebKit can do both,
///   so say so.
/// - When the session is gone, the BMC serves a stub that reads `top.lang_setting` (which the
///   main web UI defines), alerts and redirects to the logout page. Give it the setting so its
///   strings load, and say what happened instead of bouncing to the login form.
fn console_shim() -> &'static str {
    r#"(function () {
    if (window.top !== window) return;
    try {
        var ua = navigator.userAgent;
        if (ua.indexOf("Chrome") === -1) {
            Object.defineProperty(navigator, "userAgent", { get: function () { return ua + " Chrome/120.0.0.0"; } });
        }
    } catch (e) {}
    window.lang_setting = "English";
    document.addEventListener("DOMContentLoaded", function () {
        if (document.getElementById("noVNC_canvas")) return;
        window.SessionTimeout = function () {
            document.body.innerHTML = "<p style=\"font-family: sans-serif; padding: 2em\">" +
                "The BMC session has ended. Close this tab and connect again.</p>";
        };
    });
})();"#
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_form_matches_the_web_ui() {
        let plain = login_form("ADMIN", "p@ss", false);
        assert_eq!(plain[0], ("name", "ADMIN".to_string()));
        assert_eq!(plain[1], ("pwd", "p@ss".to_string()));
        assert_eq!(plain[2], ("check", "00".to_string()));
        let encoded = login_form("ADMIN", "p@ss", true);
        assert_eq!(encoded[0].1, "QURNSU4=");
        assert_eq!(encoded[1].1, "cEBzcw==");
    }

    #[test]
    fn tells_login_replies_apart() {
        let ok = r#"<script language="JavaScript" type="text/javascript">
<!--
    self.location = "../cgi/url_redirect.cgi?url_name=mainmenu";
-->
    </script>"#;
        assert!(!login_failed(ok));
        let bad = r#"<META HTTP-EQUIV="refresh" CONTENT="0;URL=/">
    <script language="javascript" type="text/javascript">
            alert(lang.LANG_EVENT_SENSOR_SPECIFIC_EVENT_STR42_2);
        if (window != top)
            top.location.href = location.href;
    </script>"#;
        assert!(login_failed(bad));
        assert_eq!(login_error(bad), "invalid username or password");
        assert_eq!(login_error(""), "login rejected");
    }

    #[test]
    fn console_page_carries_the_session_cookie() {
        let session = Session {
            host: "192.0.2.20".into(),
            sid: "abcdefghijklmno".into(),
        };
        let page = console_page(&session);
        assert_eq!(
            page.url,
            "https://192.0.2.20/cgi/url_redirect.cgi?url_name=man_ikvm_html5_bootstrap"
        );
        assert_eq!(
            page.cookies,
            vec![Cookie {
                name: "SID".into(),
                value: "abcdefghijklmno".into(),
                http_only: true,
            }]
        );
        assert!(page.disconnect_js.is_none());
        assert!(page.script.contains("lang_setting"));
        assert!(page.script.contains("Chrome/"));
    }
}
