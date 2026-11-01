//! WebKitGTK console view (Linux).

use std::cell::Cell;
use std::rc::Rc;

use anyhow::Result;
use gtk::glib;
use gtk::prelude::*;
use webkit2gtk::{
    CookieManagerExt, SettingsExt, TLSErrorsPolicy, URIRequestExt, URIResponseExt,
    UserContentInjectedFrames, UserContentManager, UserContentManagerExt, UserScript,
    UserScriptInjectionTime, WebContext, WebContextExt, WebResourceExt, WebView, WebViewExt,
    WebsiteDataManager, WebsiteDataManagerExt,
};

use super::{Hooks, LOAD_RETRY_DELAY, LoadRetry};
use crate::bmc::ConsolePage;

pub fn init() -> Result<()> {
    Ok(())
}

#[derive(Clone)]
pub struct ConsoleView {
    view: WebView,
}

impl ConsoleView {
    /// An ephemeral view with the page's cookies and script in place, loading `page.url`.
    pub fn new(page: &ConsolePage, host: &str, hooks: Hooks) -> Result<ConsoleView> {
        // Ephemeral per tab: separate cookies per BMC session, nothing written to disk.
        let data = WebsiteDataManager::new_ephemeral();
        data.set_tls_errors_policy(TLSErrorsPolicy::Ignore);
        let context = WebContext::with_website_data_manager(&data);
        // navigator.language picks the viewer's UI language; the process locale may be "C".
        context.set_preferred_languages(&["en-US"]);
        let scripts = UserContentManager::new();
        scripts.add_script(&UserScript::new(
            &page.script,
            UserContentInjectedFrames::TopFrame,
            UserScriptInjectionTime::Start,
            &[],
            &[],
        ));
        let webview = WebView::builder()
            .web_context(&context)
            .user_content_manager(&scripts)
            .build();
        let debug = std::env::var_os("RUSTDRAC_DEBUG").is_some();
        if let Some(settings) = WebViewExt::settings(&webview) {
            settings.set_enable_developer_extras(std::env::var_os("RUSTDRAC_DEVTOOLS").is_some());
            settings.set_javascript_can_open_windows_automatically(true);
            settings.set_enable_write_console_messages_to_stdout(debug);
        }
        if debug {
            webview.connect_resource_load_started(|_, resource, request| {
                let uri = request.uri().unwrap_or_default();
                eprintln!("[req] {uri}");
                resource.connect_finished(move |r| {
                    let status = r.response().map(|r| r.status_code()).unwrap_or(0);
                    eprintln!("[{status}] {uri}");
                });
                resource.connect_failed(|r, e| {
                    eprintln!("[fail] {} {e}", r.uri().unwrap_or_default());
                });
            });
        }

        // Same-origin popups (chat, help, ...) get their own window.
        webview.connect_create(|view, _| {
            let popup = WebView::builder().related_view(view).build();
            let win = gtk::Window::new(gtk::WindowType::Toplevel);
            win.set_default_size(700, 500);
            win.add(&popup);
            let w = win.clone();
            popup.connect_ready_to_show(move |_| w.show_all());
            let w = win.clone();
            popup.connect_close(move |_| w.close());
            Some(popup.upcast())
        });

        let retry = LoadRetry::new();
        webview.connect_load_failed(move |view, _, uri, err| {
            if !retry.should_retry(uri, &err) {
                return false;
            }
            let (view, uri) = (view.clone(), uri.to_string());
            glib::timeout_add_local_once(LOAD_RETRY_DELAY, move || view.load_uri(&uri));
            true
        });

        // The viewer's "Disconnect Viewer" button ends with window.close().
        let hooks = Rc::new(hooks);
        webview.connect_close(move |_| (hooks.on_close_requested)());

        // Load the page once every session cookie is in place.
        let console_url = page.url.clone();
        let view = webview.clone();
        match data.cookie_manager() {
            Some(manager) if !page.cookies.is_empty() => {
                let pending = Rc::new(Cell::new(page.cookies.len()));
                for c in &page.cookies {
                    let mut cookie = soup::Cookie::new(&c.name, &c.value, host, "/", -1);
                    cookie.set_secure(true);
                    cookie.set_http_only(c.http_only);
                    let (view, url, pending) = (view.clone(), console_url.clone(), pending.clone());
                    manager.add_cookie(&mut cookie, None::<&gtk::gio::Cancellable>, move |res| {
                        if let Err(e) = res {
                            eprintln!("rustdrac: setting session cookie: {e}");
                        }
                        pending.set(pending.get() - 1);
                        if pending.get() == 0 {
                            view.load_uri(&url);
                        }
                    });
                }
            }
            _ => view.load_uri(&console_url),
        }
        Ok(ConsoleView { view: webview })
    }

    /// The widget to show in the tab.
    pub fn widget(&self) -> gtk::Widget {
        self.view.clone().upcast()
    }

    pub fn load_uri(&self, uri: &str) {
        self.view.load_uri(uri);
    }

    /// Runs `js` in the page; `done` gets the result as text (None on error or when the
    /// result is not a value).
    pub fn evaluate_js(&self, js: &str, done: impl FnOnce(Option<String>) + 'static) {
        self.view
            .evaluate_javascript(js, None, None, None::<&gtk::gio::Cancellable>, move |res| {
                done(
                    res.ok()
                        .map(|v| javascriptcore::ValueExt::to_str(&v).to_string()),
                )
            });
    }

    /// A PNG of the visible view.
    pub fn snapshot_png(&self, done: impl FnOnce(Result<Vec<u8>>) + 'static) {
        self.view.snapshot(
            webkit2gtk::SnapshotRegion::Visible,
            webkit2gtk::SnapshotOptions::NONE,
            None::<&gtk::gio::Cancellable>,
            move |res| {
                done((|| {
                    let surface = res?;
                    let image = gtk::cairo::ImageSurface::try_from(surface)
                        .map_err(|_| anyhow::anyhow!("snapshot is not an image"))?;
                    let mut png = Vec::new();
                    image.write_to_png(&mut png)?;
                    Ok(png)
                })())
            },
        );
    }

    /// Give keyboard focus to the page so typing goes to the server.
    pub fn focus(&self) {
        self.view.grab_focus();
    }

    /// Tear the view down so its WebSocket is gone before the widget is removed.
    pub fn close(&self) {
        self.view.try_close();
    }
}
