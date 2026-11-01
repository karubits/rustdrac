//! Windows-only glue: console attachment, COM, the app identity the shell needs, folder
//! opening and toast notifications.

use std::path::Path;

use anyhow::{Context, Result};
use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegSetValueExW,
};
use windows::Win32::UI::Shell::{SetCurrentProcessExplicitAppUserModelID, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, PCWSTR, w};

/// The Application User Model ID: groups the taskbar button with the Start-menu shortcut
/// (which carries the same id) and names the sender of toast notifications.
pub const APP_ID: &str = "Karubits.RustDrac";

/// Called first thing in `main`, before GTK.
pub fn startup() {
    // SAFETY: plain Win32 calls with valid arguments, before any other thread exists.
    unsafe {
        // A GUI-subsystem exe has no console; borrow the parent's so --help and errors show
        // up when run from a terminal (after the prompt, as the shell does not wait).
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        // WebView2 needs a single-threaded apartment on the UI thread.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let _ = SetCurrentProcessExplicitAppUserModelID(&HSTRING::from(APP_ID));
    }
    if let Err(e) = register_app_id() {
        eprintln!("rustdrac: registering the app id: {e:#}");
    }
}

/// Lets the shell show toasts from an exe that is not installed as a package (the
/// installer's shortcut does the same; this covers the portable zip).
fn register_app_id() -> Result<()> {
    let exe = std::env::current_exe().context("own path")?;
    let path = HSTRING::from(format!("Software\\Classes\\AppUserModelId\\{APP_ID}"));
    let mut key = HKEY::default();
    // SAFETY: valid, NUL-terminated strings; the key handle is closed below.
    unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            &path,
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        )
        .ok()
        .context("creating the AppUserModelId key")?;
        let set = |name: PCWSTR, value: &str| {
            let wide: Vec<u16> = value.encode_utf16().chain([0]).collect();
            let bytes = std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2);
            RegSetValueExW(key, name, None, REG_SZ, Some(bytes)).ok()
        };
        let result = set(w!("DisplayName"), "Rust Drac")
            .and_then(|_| set(w!("IconUri"), &exe.display().to_string()));
        let _ = RegCloseKey(key);
        result.context("writing the AppUserModelId values")
    }
}

/// Explorer window on `dir`.
pub fn open_folder(dir: &Path) {
    let dir = HSTRING::from(dir.as_os_str());
    // SAFETY: valid strings; ShellExecuteW does not keep them.
    let r = unsafe {
        ShellExecuteW(
            None,
            w!("explore"),
            &dir,
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // Documented: a value above 32 is success.
    if r.0 as usize <= 32 {
        eprintln!(
            "rustdrac: opening {}: ShellExecute code {}",
            dir, r.0 as usize
        );
    }
}

/// Toast notification from the action centre.
pub fn notify(summary: &str, body: &str, critical: bool) -> Result<()> {
    let scenario = if critical { "urgent" } else { "default" };
    let xml = format!(
        r#"<toast scenario="{scenario}"><visual><binding template="ToastGeneric"><text>{}</text><text>{}</text></binding></visual></toast>"#,
        xml_escape(summary),
        xml_escape(body)
    );
    let doc = XmlDocument::new()?;
    doc.LoadXml(&HSTRING::from(xml))?;
    let toast = ToastNotification::CreateToastNotification(&doc)?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID))?.Show(&toast)?;
    Ok(())
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
