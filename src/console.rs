//! One iDRAC virtual console, shown as a notebook tab.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::Duration;

use anyhow::Result;
use gtk::glib;
use gtk::prelude::*;
use reqwest::blocking::Client;
use webkit2gtk::{
    CookieManagerExt, SettingsExt, TLSErrorsPolicy, URIRequestExt, URIResponseExt,
    UserContentInjectedFrames, UserContentManager, UserContentManagerExt, UserScript,
    UserScriptInjectionTime, WebContext, WebContextExt, WebResourceExt, WebView, WebViewExt,
    WebsiteDataManager, WebsiteDataManagerExt,
};

use crate::idrac;

const LANG: &str = "en";

const DISCONNECT_GRACE: Duration = Duration::from_millis(1500);
const MAX_LOAD_RETRIES: u32 = 3;

/// Where to connect and as whom.
#[derive(Debug, Clone)]
pub struct Target {
    /// Tab label.
    pub name: String,
    pub host: String,
    pub user: String,
    pub password: String,
    /// The address-book entry this came from, if any.
    pub connection_id: Option<String>,
}

type IdentityHook = Box<dyn Fn(&Target, &idrac::Identity)>;
type PageHook = Box<dyn Fn(&gtk::Widget)>;
type TitleHook = Box<dyn Fn(&gtk::Widget, &str)>;

pub struct Hooks {
    /// Redfish told us the service tag / model (used to fill in the address book).
    pub on_identity: IdentityHook,
    /// The tab has disconnected and logged out; remove its page.
    pub on_closed: PageHook,
    /// The window title for this tab changed.
    pub on_title: TitleHook,
}

enum State {
    Connecting,
    Connected {
        session: idrac::Session,
        disconnect_js: Option<&'static str>,
    },
    Failed,
    /// Closed while still logging in; the login task will log out and finish.
    ClosingWhileConnecting,
    Closing,
    Closed,
}

struct Inner {
    target: Target,
    client: Client,
    hooks: Hooks,
    state: RefCell<State>,
    title: RefCell<String>,
    stack: gtk::Stack,
    status: gtk::Label,
    error: gtk::Label,
    spinner: gtk::Spinner,
    tab_label: gtk::Label,
    tab_spinner: gtk::Spinner,
    tab: gtk::Box,
    webview: RefCell<Option<WebView>>,
}

#[derive(Clone)]
pub struct ConsoleTab(Rc<Inner>);

struct Connected {
    session: idrac::Session,
    identity: Option<idrac::Identity>,
    page: idrac::ConsolePage,
}

impl ConsoleTab {
    pub fn open(target: Target, client: Client, hooks: Hooks) -> ConsoleTab {
        let stack = gtk::Stack::new();

        // "Connecting…" page.
        let spinner = gtk::Spinner::new();
        spinner.set_size_request(32, 32);
        let status = gtk::Label::new(None);
        status.style_context().add_class("connecting-status");
        let connecting = centered_box();
        connecting.add(&spinner);
        connecting.add(&status);
        stack.add_named(&connecting, "connecting");

        // Error page.
        let error = gtk::Label::builder()
            .wrap(true)
            .selectable(true)
            .max_width_chars(80)
            .justify(gtk::Justification::Center)
            .build();
        error.style_context().add_class("error-message");
        let retry = gtk::Button::with_label("Retry");
        retry.style_context().add_class("suggested-action");
        let close = gtk::Button::with_label("Close Tab");
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_halign(gtk::Align::Center);
        buttons.add(&retry);
        buttons.add(&close);
        let failed = centered_box();
        failed.add(&gtk::Image::from_icon_name(
            Some("dialog-error-symbolic"),
            gtk::IconSize::Dialog,
        ));
        failed.add(&error);
        failed.add(&buttons);
        stack.add_named(&failed, "error");

        // Tab label: spinner, name, close button.
        let tab_spinner = gtk::Spinner::new();
        let tab_label = gtk::Label::new(Some(&target.name));
        let tab_close =
            gtk::Button::from_icon_name(Some("window-close-symbolic"), gtk::IconSize::Menu);
        tab_close.set_relief(gtk::ReliefStyle::None);
        tab_close.set_tooltip_text(Some("Disconnect and close"));
        let tab = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        tab.add(&gtk::Image::from_icon_name(
            Some("computer-symbolic"),
            gtk::IconSize::Menu,
        ));
        tab.add(&tab_spinner);
        tab.add(&tab_label);
        tab.add(&tab_close);
        tab.show_all();

        let this = ConsoleTab(Rc::new(Inner {
            title: RefCell::new(target.name.clone()),
            target,
            client,
            hooks,
            state: RefCell::new(State::Connecting),
            stack,
            status,
            error,
            spinner,
            tab_label,
            tab_spinner,
            tab,
            webview: RefCell::new(None),
        }));

        let weak = this.downgrade();
        retry.connect_clicked(move |_| {
            if let Some(tab) = upgrade(&weak) {
                tab.connect();
            }
        });
        for button in [&close, &tab_close] {
            let weak = this.downgrade();
            button.connect_clicked(move |_| {
                if let Some(tab) = upgrade(&weak) {
                    tab.close();
                }
            });
        }

        this.0.stack.show_all();
        this.connect();
        this
    }

    pub fn widget(&self) -> gtk::Widget {
        self.0.stack.clone().upcast()
    }

    pub fn tab_widget(&self) -> gtk::Widget {
        self.0.tab.clone().upcast()
    }

    pub fn title(&self) -> String {
        self.0.title.borrow().clone()
    }

    pub fn webview(&self) -> Option<WebView> {
        self.0.webview.borrow().clone()
    }

    pub fn is_closed(&self) -> bool {
        matches!(*self.0.state.borrow(), State::Closed)
    }

    /// Give keyboard focus to the console so typing goes to the server.
    pub fn focus(&self) {
        if let Some(view) = self.webview() {
            view.grab_focus();
        }
    }

    fn downgrade(&self) -> Weak<Inner> {
        Rc::downgrade(&self.0)
    }

    fn set_busy(&self, busy: bool) {
        for spinner in [&self.0.spinner, &self.0.tab_spinner] {
            if busy {
                spinner.start();
                spinner.show();
            } else {
                spinner.stop();
                spinner.hide();
            }
        }
    }

    fn connect(&self) {
        let t = &self.0.target;
        *self.0.state.borrow_mut() = State::Connecting;
        self.0
            .status
            .set_text(&format!("Connecting to {} as {}…", t.host, t.user));
        self.0.stack.set_visible_child_name("connecting");
        self.set_busy(true);

        let (client, target) = (self.0.client.clone(), t.clone());
        let job = gtk::gio::spawn_blocking(move || connect_blocking(&client, &target));
        let weak = self.downgrade();
        glib::MainContext::default().spawn_local(async move {
            let result = job
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("login task panicked")));
            if let Some(tab) = upgrade(&weak) {
                tab.connected(result);
            }
        });
    }

    fn connected(&self, result: Result<Connected>) {
        self.set_busy(false);
        let closing = matches!(*self.0.state.borrow(), State::ClosingWhileConnecting);
        match result {
            Ok(c) if closing => {
                *self.0.state.borrow_mut() = State::Closing;
                self.logout_and_finish(c.session);
            }
            Err(_) if closing => self.finish(),
            Err(e) => {
                eprintln!("rustdrac: {}: {e:#}", self.0.target.host);
                *self.0.state.borrow_mut() = State::Failed;
                self.0.error.set_text(&format!("{e:#}"));
                self.0.stack.set_visible_child_name("error");
            }
            Ok(c) => {
                if let Some(identity) = &c.identity {
                    (self.0.hooks.on_identity)(&self.0.target, identity);
                }
                let title = c
                    .identity
                    .as_ref()
                    .and_then(|i| i.title())
                    .unwrap_or_else(|| self.0.target.host.clone());
                *self.0.title.borrow_mut() = title.clone();
                (self.0.hooks.on_title)(&self.widget(), &title);

                let view = self.build_webview(&c.session, &c.page);
                self.0.stack.add_named(&view, "console");
                view.show();
                self.0.stack.set_visible_child_name("console");
                *self.0.webview.borrow_mut() = Some(view);
                *self.0.state.borrow_mut() = State::Connected {
                    session: c.session,
                    disconnect_js: c.page.disconnect_js,
                };
                self.focus();
            }
        }
    }

    /// Disconnect the console and log out, then remove the tab.
    ///
    /// On iDRAC 8, just dropping the KVM WebSocket leaves a stale console session ("A session
    /// to this device already exists"), so disconnect the way the viewer's "Disconnect Viewer"
    /// button does. iDRAC 9's viewer has no global hook; unloading the page closes its socket.
    /// Either way, give it a moment to reach the iDRAC, then log out.
    pub fn close(&self) {
        let state = std::mem::replace(&mut *self.0.state.borrow_mut(), State::Closing);
        match state {
            State::Connecting => {
                *self.0.state.borrow_mut() = State::ClosingWhileConnecting;
                self.0.status.set_text("Cancelling…");
            }
            State::Connected {
                session,
                disconnect_js,
            } => {
                self.0
                    .tab_label
                    .set_text(&format!("{} (closing)", self.0.target.name));
                self.set_busy(true);
                if let Some(view) = self.webview() {
                    match disconnect_js {
                        Some(js) => view.evaluate_javascript(
                            js,
                            None,
                            None,
                            None::<&gtk::gio::Cancellable>,
                            |_| {},
                        ),
                        None => view.load_uri("about:blank"),
                    }
                }
                let weak = self.downgrade();
                glib::timeout_add_local_once(DISCONNECT_GRACE, move || {
                    if let Some(tab) = upgrade(&weak) {
                        tab.logout_and_finish(session);
                    }
                });
            }
            State::Failed => self.finish(),
            s @ (State::ClosingWhileConnecting | State::Closing | State::Closed) => {
                *self.0.state.borrow_mut() = s;
            }
        }
    }

    fn logout_and_finish(&self, session: idrac::Session) {
        let client = self.0.client.clone();
        let job = gtk::gio::spawn_blocking(move || idrac::logout(&client, &session));
        let weak = self.downgrade();
        glib::MainContext::default().spawn_local(async move {
            if let Ok(Err(e)) = job.await {
                eprintln!("rustdrac: {e:#}");
            }
            if let Some(tab) = upgrade(&weak) {
                tab.finish();
            }
        });
    }

    fn finish(&self) {
        *self.0.state.borrow_mut() = State::Closed;
        self.set_busy(false);
        // Drop the web view first so its WebSocket is gone before the page is removed.
        if let Some(view) = self.0.webview.borrow_mut().take() {
            view.try_close();
        }
        (self.0.hooks.on_closed)(&self.widget());
    }

    fn build_webview(&self, session: &idrac::Session, page: &idrac::ConsolePage) -> WebView {
        // Ephemeral per tab: separate cookies per iDRAC session, nothing written to disk.
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

        // The iDRAC web server occasionally drops connections; retry the console page a few times.
        let retries = Cell::new(0);
        webview.connect_load_failed(move |view, _, uri, err| {
            if retries.get() >= MAX_LOAD_RETRIES || uri.is_empty() {
                return false;
            }
            retries.set(retries.get() + 1);
            eprintln!("rustdrac: loading console failed ({err}), retrying");
            let (view, uri) = (view.clone(), uri.to_string());
            glib::timeout_add_local_once(Duration::from_secs(2), move || view.load_uri(&uri));
            true
        });

        // The viewer's "Disconnect Viewer" button ends with window.close().
        let weak = self.downgrade();
        webview.connect_close(move |_| {
            if let Some(tab) = upgrade(&weak) {
                tab.close();
            }
        });

        let console_url = page.url.clone();
        let mut cookie = soup::Cookie::new(
            idrac::SESSION_COOKIE,
            &session.cookie,
            &session.host,
            "/",
            -1,
        );
        cookie.set_secure(true);
        cookie.set_http_only(true);
        let view = webview.clone();
        match data.cookie_manager() {
            Some(cookies) => {
                cookies.add_cookie(&mut cookie, None::<&gtk::gio::Cancellable>, move |res| {
                    if let Err(e) = res {
                        eprintln!("rustdrac: setting session cookie: {e}");
                    }
                    view.load_uri(&console_url);
                })
            }
            None => view.load_uri(&console_url),
        }
        webview
    }
}

fn upgrade(weak: &Weak<Inner>) -> Option<ConsoleTab> {
    weak.upgrade().map(ConsoleTab)
}

fn centered_box() -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 12);
    b.set_halign(gtk::Align::Center);
    b.set_valign(gtk::Align::Center);
    b
}

fn connect_blocking(client: &Client, target: &Target) -> Result<Connected> {
    let session = idrac::login(client, &target.host, &target.user, &target.password)?;
    let identity = idrac::identity(client, &target.host, &target.user, &target.password);
    let title = identity
        .as_ref()
        .and_then(|i| i.title())
        .unwrap_or_else(|| target.host.clone());
    match idrac::console_page(client, &session, &title, LANG) {
        Ok(page) => Ok(Connected {
            session,
            identity,
            page,
        }),
        Err(e) => {
            let _ = idrac::logout(client, &session);
            Err(e)
        }
    }
}
