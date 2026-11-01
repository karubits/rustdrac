//! Microsoft WebView2 console view (Windows).
//!
//! WebView2 is not a GTK widget: it is a native child window of the GTK toplevel's HWND.
//! A `gtk::DrawingArea` stands in for it in the tab's Stack; its allocation and map state
//! drive the child window's bounds and visibility. The browser engine (Edge) is shared
//! with the system; one environment per process, one in-private profile per tab.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use gtk::glib;
use gtk::glib::translate::ToGlibPtr;
use gtk::prelude::*;
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{
    AddScriptToExecuteOnDocumentCreatedCompletedHandler,
    CallDevToolsProtocolMethodCompletedHandler, CapturePreviewCompletedHandler,
    CoreWebView2EnvironmentOptions, CreateCoreWebView2ControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler, DevToolsProtocolEventReceivedEventHandler,
    ExecuteScriptCompletedHandler, NavigationCompletedEventHandler,
    ServerCertificateErrorDetectedEventHandler, WebResourceResponseReceivedEventHandler,
    WindowCloseRequestedEventHandler, take_pwstr,
};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::System::Com::{IStream, STREAM_SEEK_SET};
use windows::Win32::UI::Shell::SHCreateMemStream;
use windows::core::{HSTRING, Interface, PCWSTR, PWSTR, w};

use super::{Hooks, LOAD_RETRY_DELAY, LoadRetry};
use crate::bmc::{ConsolePage, Cookie};

const RUNTIME_MISSING: &str = "The Microsoft Edge WebView2 Runtime is not installed. Install it from \
     https://developer.microsoft.com/microsoft-edge/webview2/consumer/ and open the console again.";

thread_local! {
    /// The shared browser environment, or why there is none. Only used on the GTK thread.
    static ENV: RefCell<Option<std::result::Result<ICoreWebView2Environment, String>>> =
        const { RefCell::new(None) };
}

static NEXT_PROFILE: AtomicU32 = AtomicU32::new(1);

// Exported by libgdk-3 on Windows, but not declared in gdk-sys.
#[link(name = "gdk-3")]
unsafe extern "C" {
    fn gdk_win32_window_get_handle(window: *mut gtk::gdk::ffi::GdkWindow) -> *mut c_void;
}

fn debug() -> bool {
    std::env::var_os("RUSTDRAC_DEBUG").is_some()
}

/// Checks for the runtime and creates the environment (a short nested message loop; this
/// runs once at startup, before any window exists).
pub fn init() -> Result<()> {
    let outcome = create_environment();
    let stored = outcome.as_ref().map_err(|e| format!("{e:#}")).cloned();
    ENV.with(|env| *env.borrow_mut() = Some(stored));
    outcome.map(|_| ())
}

fn create_environment() -> Result<ICoreWebView2Environment> {
    let mut version = PWSTR::null();
    // SAFETY: a valid out-pointer; the string it returns is freed by take_pwstr.
    unsafe { GetAvailableCoreWebView2BrowserVersionString(PCWSTR::null(), &mut version) }
        .map_err(|_| anyhow!(RUNTIME_MISSING))?;
    if version.is_null() {
        bail!(RUNTIME_MISSING);
    }
    let version = take_pwstr(version);
    if debug() {
        eprintln!("rustdrac: WebView2 runtime {version}");
    }
    let folder = dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("rustdrac")
        .join("WebView2");
    let options = CoreWebView2EnvironmentOptions::default();
    // SAFETY: the options object is ours and not yet shared with the browser.
    unsafe {
        // navigator.language picks the viewer's UI language.
        options.set_language("en-US".into());
        // BMCs have self-signed certificates, also on the KVM's own port (iDRAC 8 uses 5900
        // for its WebSocket); the per-request certificate hook below covers page loads, this
        // covers everything else.
        options.set_additional_browser_arguments("--ignore-certificate-errors".into());
    }
    let options: ICoreWebView2EnvironmentOptions = options.into();
    let folder = HSTRING::from(folder.as_os_str());
    let (tx, rx) = std::sync::mpsc::channel();
    CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| {
            // SAFETY: valid arguments; the handler keeps the browser's reference alive.
            unsafe {
                CreateCoreWebView2EnvironmentWithOptions(
                    PCWSTR::null(),
                    &folder,
                    &options,
                    &handler,
                )
            }
            .map_err(Into::into)
        }),
        Box::new(move |hr, env| {
            hr?;
            let _ = tx.send(env);
            Ok(())
        }),
    )
    .map_err(|e| anyhow!("starting WebView2: {e:?}"))?;
    rx.try_recv()
        .ok()
        .flatten()
        .context("WebView2 did not return an environment")
}

/// The page to load once the controller exists.
struct Pending {
    url: String,
    script: String,
    cookies: Vec<Cookie>,
    host: String,
}

struct Inner {
    area: gtk::DrawingArea,
    hooks: Hooks,
    pending: RefCell<Option<Pending>>,
    controller: RefCell<Option<ICoreWebView2Controller>>,
    webview: RefCell<Option<ICoreWebView2>>,
    retry: LoadRetry,
    /// What was last navigated to, for retries.
    uri: RefCell<String>,
    closed: Cell<bool>,
}

#[derive(Clone)]
pub struct ConsoleView(Rc<Inner>);

impl ConsoleView {
    /// A view in its own in-private profile. The page loads once the placeholder widget is
    /// realised inside the window.
    pub fn new(page: &ConsolePage, host: &str, hooks: Hooks) -> Result<ConsoleView> {
        ENV.with(|env| match &*env.borrow() {
            Some(Ok(_)) => Ok(()),
            Some(Err(e)) => Err(anyhow!("{e}")),
            None => Err(anyhow!("WebView2 was not initialised")),
        })?;
        let area = gtk::DrawingArea::new();
        area.set_can_focus(true);
        let inner = Rc::new(Inner {
            area: area.clone(),
            hooks,
            pending: RefCell::new(Some(Pending {
                url: page.url.clone(),
                script: page.script.clone(),
                cookies: page.cookies.clone(),
                host: host.to_string(),
            })),
            controller: RefCell::new(None),
            webview: RefCell::new(None),
            retry: LoadRetry::new(),
            uri: RefCell::new(String::new()),
            closed: Cell::new(false),
        });

        let weak = Rc::downgrade(&inner);
        area.connect_realize(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.create_controller();
            }
        });
        let weak = Rc::downgrade(&inner);
        area.connect_size_allocate(move |_, _| {
            if let Some(inner) = weak.upgrade() {
                inner.apply_bounds();
            }
        });
        let weak = Rc::downgrade(&inner);
        area.connect_map(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.set_visible(true);
            }
        });
        let weak = Rc::downgrade(&inner);
        area.connect_unmap(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.set_visible(false);
            }
        });
        let weak = Rc::downgrade(&inner);
        area.connect_focus_in_event(move |_, _| {
            if let Some(inner) = weak.upgrade() {
                inner.focus();
            }
            glib::Propagation::Proceed
        });
        let weak = Rc::downgrade(&inner);
        area.connect_destroy(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.close();
            }
        });
        Ok(ConsoleView(inner))
    }

    pub fn widget(&self) -> gtk::Widget {
        self.0.area.clone().upcast()
    }

    pub fn load_uri(&self, uri: &str) {
        *self.0.uri.borrow_mut() = uri.to_string();
        if let Some(wv) = self.0.webview.borrow().as_ref() {
            // SAFETY: a valid string; the browser copies it.
            if let Err(e) = unsafe { wv.Navigate(&HSTRING::from(uri)) } {
                eprintln!("rustdrac: navigating to {uri}: {e}");
            }
        }
    }

    /// Runs `js` in the page; `done` gets a string result as is, any other value as JSON
    /// text, and None for null/undefined or an error.
    pub fn evaluate_js(&self, js: &str, done: impl FnOnce(Option<String>) + 'static) {
        let Some(wv) = self.0.webview.borrow().clone() else {
            return done(None);
        };
        let done = Rc::new(RefCell::new(Some(done)));
        let d = done.clone();
        let handler = ExecuteScriptCompletedHandler::create(Box::new(move |hr, json| {
            if let Some(done) = d.borrow_mut().take() {
                done(if hr.is_ok() { json_text(&json) } else { None });
            }
            Ok(())
        }));
        // SAFETY: valid arguments; the browser keeps the handler alive until it completes.
        if let Err(e) = unsafe { wv.ExecuteScript(&HSTRING::from(js), &handler) } {
            eprintln!("rustdrac: running script: {e}");
            if let Some(done) = done.borrow_mut().take() {
                done(None);
            }
        }
    }

    /// A PNG of the visible view.
    pub fn snapshot_png(&self, done: impl FnOnce(Result<Vec<u8>>) + 'static) {
        let Some(wv) = self.0.webview.borrow().clone() else {
            return done(Err(anyhow!("the console is not loaded")));
        };
        // SAFETY: a fresh, empty memory stream.
        let Some(stream) = (unsafe { SHCreateMemStream(None) }) else {
            return done(Err(anyhow!("cannot create a memory stream")));
        };
        let done = Rc::new(RefCell::new(Some(done)));
        let (d, s) = (done.clone(), stream.clone());
        let handler = CapturePreviewCompletedHandler::create(Box::new(move |hr| {
            if let Some(done) = d.borrow_mut().take() {
                done(hr.map_err(Into::into).and_then(|_| read_all(&s)));
            }
            Ok(())
        }));
        // SAFETY: valid arguments; the browser keeps the stream and handler alive.
        let started = unsafe {
            wv.CapturePreview(
                COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG,
                &stream,
                &handler,
            )
        };
        if let Err(e) = started
            && let Some(done) = done.borrow_mut().take()
        {
            done(Err(anyhow!("capturing the page: {e}")));
        }
    }

    /// Give keyboard focus to the page so typing goes to the server.
    pub fn focus(&self) {
        self.0.focus();
    }

    /// Tear the browser down. Idempotent.
    pub fn close(&self) {
        self.0.close();
    }
}

impl Inner {
    fn create_controller(self: &Rc<Self>) {
        if self.closed.get() || self.controller.borrow().is_some() {
            return;
        }
        let env = ENV.with(|env| match &*env.borrow() {
            Some(Ok(env)) => Some(env.clone()),
            _ => None,
        });
        let Some(env) = env else {
            return self.fail("WebView2 is not available".into());
        };
        let hwnd = match toplevel_hwnd(&self.area) {
            Ok(hwnd) => hwnd,
            Err(e) => return self.fail(format!("{e:#}")),
        };
        let weak = Rc::downgrade(self);
        let handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(
            move |hr, controller| {
                let Some(inner) = weak.upgrade() else {
                    // The tab went away meanwhile; do not leak a browser.
                    if let Some(c) = controller {
                        // SAFETY: a controller nobody else holds.
                        let _ = unsafe { c.Close() };
                    }
                    return Ok(());
                };
                let result = hr
                    .and_then(|_| controller.ok_or_else(windows::core::Error::empty))
                    .and_then(|c| inner.attach(c));
                if let Err(e) = result {
                    inner.fail(format!("starting the WebView2 browser: {e}"));
                }
                Ok(())
            },
        ));
        let profile = format!("console-{}", NEXT_PROFILE.fetch_add(1, Ordering::Relaxed));
        // SAFETY: valid arguments; the handler keeps the browser's reference alive.
        let started = unsafe {
            match env.cast::<ICoreWebView2Environment10>() {
                Ok(env10) => env10
                    .CreateCoreWebView2ControllerOptions()
                    .and_then(|opts| {
                        // Like WebKit's ephemeral data manager: own cookies, nothing on disk.
                        opts.SetProfileName(&HSTRING::from(profile))?;
                        opts.SetIsInPrivateModeEnabled(true)?;
                        env10.CreateCoreWebView2ControllerWithOptions(hwnd, &opts, &handler)
                    }),
                Err(_) => env.CreateCoreWebView2Controller(hwnd, &handler),
            }
        };
        if let Err(e) = started {
            self.fail(format!("starting the WebView2 browser: {e}"));
        }
    }

    /// The controller exists: configure it, install the page's cookies and script, load.
    fn attach(self: &Rc<Self>, controller: ICoreWebView2Controller) -> windows::core::Result<()> {
        let debug = debug();
        let devtools = std::env::var_os("RUSTDRAC_DEVTOOLS").is_some();
        // SAFETY: COM calls on live objects with valid arguments, all on the GTK thread.
        unsafe {
            let wv = controller.CoreWebView2()?;
            let settings = wv.Settings()?;
            settings.SetAreDevToolsEnabled(devtools)?;
            // The context menu is how DevTools are opened; the KVM has no use for it.
            settings.SetAreDefaultContextMenusEnabled(devtools)?;
            settings.SetIsStatusBarEnabled(false)?;
            settings.SetIsZoomControlEnabled(false)?;
            settings.SetIsBuiltInErrorPageEnabled(false)?;
            if let Ok(s3) = settings.cast::<ICoreWebView2Settings3>() {
                // F5, Ctrl+F, Ctrl+P... belong to the server, not the browser.
                s3.SetAreBrowserAcceleratorKeysEnabled(false)?;
            }
            if let Ok(c2) = controller.cast::<ICoreWebView2Controller2>() {
                let _ = c2.SetDefaultBackgroundColor(COREWEBVIEW2_COLOR {
                    A: 255,
                    R: 0,
                    G: 0,
                    B: 0,
                });
            }

            let mut token = 0i64;
            // The viewer's "Disconnect Viewer" button ends with window.close().
            let weak = Rc::downgrade(self);
            wv.add_WindowCloseRequested(
                &WindowCloseRequestedEventHandler::create(Box::new(move |_, _| {
                    let weak = weak.clone();
                    glib::idle_add_local_once(move || {
                        if let Some(inner) = weak.upgrade() {
                            (inner.hooks.on_close_requested)();
                        }
                    });
                    Ok(())
                })),
                &mut token,
            )?;
            let weak = Rc::downgrade(self);
            wv.add_NavigationCompleted(
                &NavigationCompletedEventHandler::create(Box::new(move |_, args| {
                    if let (Some(inner), Some(args)) = (weak.upgrade(), args) {
                        inner.navigation_completed(&args);
                    }
                    Ok(())
                })),
                &mut token,
            )?;
            if let Ok(wv14) = wv.cast::<ICoreWebView2_14>() {
                wv14.add_ServerCertificateErrorDetected(
                    &ServerCertificateErrorDetectedEventHandler::create(Box::new(|_, args| {
                        if let Some(args) = args {
                            args.SetAction(
                                COREWEBVIEW2_SERVER_CERTIFICATE_ERROR_ACTION_ALWAYS_ALLOW,
                            )?;
                        }
                        Ok(())
                    })),
                    &mut token,
                )?;
            }
            if debug {
                self.enable_debug_logging(&wv)?;
            }

            if let Some(page) = self.pending.borrow_mut().take() {
                let cookies = wv.cast::<ICoreWebView2_2>()?.CookieManager()?;
                for c in &page.cookies {
                    let cookie = cookies.CreateCookie(
                        &HSTRING::from(&c.name),
                        &HSTRING::from(&c.value),
                        &HSTRING::from(&page.host),
                        w!("/"),
                    )?;
                    cookie.SetIsSecure(true)?;
                    cookie.SetIsHttpOnly(c.http_only)?;
                    cookies.AddOrUpdateCookie(&cookie)?;
                }
                // Unlike WebKit's TopFrame scripts this runs in every frame, hence the guard.
                // It is only in place once its handler ran, so navigate from there.
                let script = format!("if (window === window.top) {{\n{}\n}}", page.script);
                *self.uri.borrow_mut() = page.url.clone();
                let (view, url) = (wv.clone(), page.url);
                wv.AddScriptToExecuteOnDocumentCreated(
                    &HSTRING::from(script),
                    &AddScriptToExecuteOnDocumentCreatedCompletedHandler::create(Box::new(
                        move |hr, _| {
                            if let Err(e) = hr {
                                eprintln!("rustdrac: installing the console script: {e}");
                            }
                            view.Navigate(&HSTRING::from(url))
                        },
                    )),
                )?;
            }

            *self.webview.borrow_mut() = Some(wv);
            *self.controller.borrow_mut() = Some(controller);
        }
        self.apply_bounds();
        self.set_visible(self.area.is_mapped());
        Ok(())
    }

    /// `RUSTDRAC_DEBUG`: every response's status and the page's console output.
    unsafe fn enable_debug_logging(&self, wv: &ICoreWebView2) -> windows::core::Result<()> {
        // SAFETY: as in attach.
        unsafe {
            let mut token = 0i64;
            wv.cast::<ICoreWebView2_2>()?
                .add_WebResourceResponseReceived(
                    &WebResourceResponseReceivedEventHandler::create(Box::new(|_, args| {
                        if let Some(args) = args {
                            let mut uri = PWSTR::null();
                            args.Request()?.Uri(&mut uri)?;
                            let mut status = 0i32;
                            args.Response()?.StatusCode(&mut status)?;
                            eprintln!("[{status}] {}", take_pwstr(uri));
                        }
                        Ok(())
                    })),
                    &mut token,
                )?;
            wv.CallDevToolsProtocolMethod(
                w!("Runtime.enable"),
                w!("{}"),
                &CallDevToolsProtocolMethodCompletedHandler::create(Box::new(|_, _| Ok(()))),
            )?;
            wv.GetDevToolsProtocolEventReceiver(w!("Runtime.consoleAPICalled"))?
                .add_DevToolsProtocolEventReceived(
                    &DevToolsProtocolEventReceivedEventHandler::create(Box::new(|_, args| {
                        if let Some(args) = args {
                            let mut json = PWSTR::null();
                            args.ParameterObjectAsJson(&mut json)?;
                            eprintln!("[console] {}", console_message(&take_pwstr(json)));
                        }
                        Ok(())
                    })),
                    &mut token,
                )?;
        }
        Ok(())
    }

    fn navigation_completed(self: &Rc<Self>, args: &ICoreWebView2NavigationCompletedEventArgs) {
        let mut ok = windows::core::BOOL::default();
        let mut status = COREWEBVIEW2_WEB_ERROR_STATUS_UNKNOWN;
        // SAFETY: valid out-pointers on a live args object.
        unsafe {
            let _ = args.IsSuccess(&mut ok);
            let _ = args.WebErrorStatus(&mut status);
        }
        if ok.as_bool() {
            return;
        }
        // A certificate error is answered by the ServerCertificateErrorDetected hook, after
        // which the browser retries on its own; a cancelled navigation was replaced by a
        // newer one.
        if matches!(
            status,
            COREWEBVIEW2_WEB_ERROR_STATUS_OPERATION_CANCELED
                | COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_COMMON_NAME_IS_INCORRECT
                | COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_EXPIRED
                | COREWEBVIEW2_WEB_ERROR_STATUS_CLIENT_CERTIFICATE_CONTAINS_ERRORS
                | COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_REVOKED
                | COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_IS_INVALID
        ) {
            return;
        }
        let uri = self.uri.borrow().clone();
        let err = format!("web error status {}", status.0);
        if self.retry.should_retry(&uri, &err) {
            let weak = Rc::downgrade(self);
            glib::timeout_add_local_once(LOAD_RETRY_DELAY, move || {
                if let Some(inner) = weak.upgrade() {
                    ConsoleView(inner).load_uri(&uri);
                }
            });
        } else if !uri.is_empty() {
            self.fail(format!("loading the console page failed ({err})"));
        }
    }

    /// Report an unusable view to the tab, outside the current callback.
    fn fail(self: &Rc<Self>, message: String) {
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(inner) = weak.upgrade() {
                (inner.hooks.on_load_failed)(message);
            }
        });
    }

    /// Place the child window over the placeholder: its position relative to the toplevel's
    /// client area, in physical pixels.
    fn apply_bounds(&self) {
        let Some(controller) = self.controller.borrow().clone() else {
            return;
        };
        let (Some(top), Some(own)) = (
            self.area.toplevel().and_then(|t| t.window()),
            self.area.window(),
        ) else {
            return;
        };
        let (tx, ty) = top.root_coords(0, 0);
        let (ax, ay) = own.root_coords(0, 0);
        let alloc = self.area.allocation();
        let scale = self.area.scale_factor();
        let (x, y) = (ax - tx, ay - ty);
        let bounds = RECT {
            left: x * scale,
            top: y * scale,
            right: (x + alloc.width()) * scale,
            bottom: (y + alloc.height()) * scale,
        };
        if debug() {
            let check = self
                .area
                .translate_coordinates(&self.area.toplevel().unwrap(), 0, 0);
            eprintln!(
                "rustdrac: console bounds {x},{y} {}x{} scale {scale} (widget coords {check:?})",
                alloc.width(),
                alloc.height()
            );
        }
        // SAFETY: a live controller.
        if let Err(e) = unsafe { controller.SetBounds(bounds) } {
            eprintln!("rustdrac: placing the console: {e}");
        }
    }

    fn set_visible(&self, visible: bool) {
        if let Some(controller) = self.controller.borrow().as_ref() {
            // SAFETY: a live controller.
            let _ = unsafe { controller.SetIsVisible(visible) };
        }
    }

    fn focus(&self) {
        if let Some(controller) = self.controller.borrow().as_ref() {
            // SAFETY: a live controller.
            let _ = unsafe { controller.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC) };
        }
    }

    fn close(&self) {
        if self.closed.replace(true) {
            return;
        }
        self.webview.borrow_mut().take();
        if let Some(controller) = self.controller.borrow_mut().take() {
            // SAFETY: a live controller; Close releases the browser and every handler.
            let _ = unsafe { controller.Close() };
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.close();
    }
}

/// The HWND of the GTK toplevel that `widget` sits in.
fn toplevel_hwnd(widget: &impl IsA<gtk::Widget>) -> Result<HWND> {
    let window = widget
        .toplevel()
        .and_then(|t| t.window())
        .context("the console widget is not in a window")?;
    // SAFETY: a realised toplevel GdkWindow, which on Windows wraps an HWND.
    let handle = unsafe { gdk_win32_window_get_handle(window.to_glib_none().0) };
    if handle.is_null() {
        bail!("the window has no native handle");
    }
    Ok(HWND(handle))
}

/// `ExecuteScript` results are JSON: a string comes back as its text, null/undefined as None.
fn json_text(json: &str) -> Option<String> {
    match serde_json::from_str::<serde_json::Value>(json) {
        Ok(serde_json::Value::String(s)) => Some(s),
        Ok(serde_json::Value::Null) | Err(_) => None,
        Ok(v) => Some(v.to_string()),
    }
}

/// The text of a `Runtime.consoleAPICalled` event.
fn console_message(json: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return json.to_string();
    };
    let kind = v["type"].as_str().unwrap_or("log");
    let args: Vec<String> = v["args"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|arg| match &arg["value"] {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Null => {
                        arg["description"].as_str().unwrap_or("?").to_string()
                    }
                    other => other.to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    format!("{kind}: {}", args.join(" "))
}

fn read_all(stream: &IStream) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    // SAFETY: a stream we created; the buffer outlives each Read call.
    unsafe {
        stream.Seek(0, STREAM_SEEK_SET, None)?;
        loop {
            let mut n = 0u32;
            let hr = stream.Read(
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                Some(&mut n),
            );
            if hr.is_err() {
                bail!("reading the captured image: {hr}");
            }
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_results_are_decoded() {
        assert_eq!(
            json_text("\"data:image/png;base64,AA==\"").as_deref(),
            Some("data:image/png;base64,AA==")
        );
        assert_eq!(json_text("null"), None);
        assert_eq!(json_text("").as_deref(), None);
        assert_eq!(json_text("{\"a\":1}").as_deref(), Some("{\"a\":1}"));
    }

    #[test]
    fn console_events_are_summarised() {
        let json = r#"{"type":"error","args":[{"type":"string","value":"boom"},{"type":"number","value":7}]}"#;
        assert_eq!(console_message(json), "error: boom 7");
    }
}
