//! One BMC virtual console, shown as a notebook tab.

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::time::Duration;

use anyhow::Result;
use gtk::glib;
use gtk::prelude::*;
use reqwest::blocking::Client;

use crate::bmc;
use crate::webview::{self, ConsoleView};

const LANG: &str = "en";

const DISCONNECT_GRACE: Duration = Duration::from_millis(1500);

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

type IdentityHook = Box<dyn Fn(&Target, &bmc::Identity)>;
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
        session: bmc::Session,
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
    webview: RefCell<Option<ConsoleView>>,
    service_tag: RefCell<String>,
}

#[derive(Clone)]
pub struct ConsoleTab(Rc<Inner>);

/// Debugging aid: every canvas with its size, on-screen box and a few sampled pixels.
const CANVAS_INVENTORY_JS: &str = r##"(function () {
    var out = { frames: document.querySelectorAll("iframe").length, shadows: 0, canvases: [], big: [] };
    function walk(root, path) {
        root.querySelectorAll("*").forEach(function (el) {
            if (el.shadowRoot) { out.shadows++; walk(el.shadowRoot, path + ">" + el.tagName); }
            var r = el.getBoundingClientRect();
            if (el.tagName === "CANVAS") {
                var px = null;
                try { var ctx = el.getContext("2d"); if (ctx) px = Array.from(ctx.getImageData(el.width / 2, el.height / 2, 1, 1).data); }
                catch (e) { px = String(e); }
                out.canvases.push({ path: path, w: el.width, h: el.height, box: [Math.round(r.width), Math.round(r.height)], px: px });
            }
            if (r.width * r.height > 300000 && el.children.length < 3)
                out.big.push(el.tagName + (el.id ? "#" + el.id : "") + "." + String(el.className).slice(0, 40) + " " + Math.round(r.width) + "x" + Math.round(r.height));
        });
    }
    walk(document, "doc");
    document.querySelectorAll("iframe").forEach(function (f, i) {
        try { walk(f.contentDocument, "iframe" + i); } catch (e) { out.canvases.push("iframe" + i + ": " + e); }
    });
    return JSON.stringify(out);
})()"##;

/// The KVM picture as a PNG data URL: the viewer draws it into its largest visible
/// `<canvas>` (both the iDRAC 8 viewer and iDRAC 9's noVNC). Empty if there is none yet.
const CANVAS_PNG_JS: &str = r#"(function () {
    var best = null, area = 0;
    document.querySelectorAll("canvas").forEach(function (c) {
        var r = c.getBoundingClientRect();
        // Only a canvas that is actually on screen: before the KVM connects, the viewer has
        // a hidden, empty one.
        var a = c.width * c.height;
        if (r.width > 0 && r.height > 0 && a > area) { area = a; best = c; }
    });
    return best ? best.toDataURL("image/png") : "";
})()"#;

struct Connected {
    session: bmc::Session,
    identity: Option<bmc::Identity>,
    page: bmc::ConsolePage,
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
            service_tag: RefCell::new(String::new()),
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

    fn view(&self) -> Option<ConsoleView> {
        self.0.webview.borrow().clone()
    }

    pub fn target(&self) -> &Target {
        &self.0.target
    }

    /// Learned from Redfish on connect; empty until then.
    pub fn service_tag(&self) -> String {
        self.0.service_tag.borrow().clone()
    }

    pub fn is_connected(&self) -> bool {
        matches!(*self.0.state.borrow(), State::Connected { .. })
    }

    /// A PNG of the console: the KVM canvas without Dell's toolbar, or (if the page has no
    /// readable canvas) a snapshot of the whole view.
    pub fn screenshot(&self, done: impl FnOnce(Result<Vec<u8>>) + 'static) {
        let Some(view) = self.view().filter(|_| self.is_connected()) else {
            return done(Err(anyhow::anyhow!("the console is not connected")));
        };
        if std::env::var_os("RUSTDRAC_DEBUG").is_some() {
            view.evaluate_js(CANVAS_INVENTORY_JS, |res| {
                if let Some(v) = res {
                    eprintln!("[canvases] {v}");
                }
            });
        }
        let fallback = view.clone();
        view.evaluate_js(CANVAS_PNG_JS, move |data_url| {
            match data_url.as_deref().and_then(decode_png_data_url) {
                Some(png) => done(Ok(png)),
                None => fallback.snapshot_png(done),
            }
        });
    }

    /// A PNG of the whole console view (test aid).
    pub fn snapshot_png(&self, done: impl FnOnce(Result<Vec<u8>>) + 'static) {
        match self.view() {
            Some(view) => view.snapshot_png(done),
            None => done(Err(anyhow::anyhow!("the console is not connected"))),
        }
    }

    pub fn is_closed(&self) -> bool {
        matches!(*self.0.state.borrow(), State::Closed)
    }

    /// Give keyboard focus to the console so typing goes to the server.
    pub fn focus(&self) {
        if let Some(view) = self.view() {
            view.focus();
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
            Err(e) => self.show_error(&e),
            Ok(c) => {
                if let Some(identity) = &c.identity {
                    *self.0.service_tag.borrow_mut() = identity.service_tag.clone();
                    (self.0.hooks.on_identity)(&self.0.target, identity);
                }
                let title = c
                    .identity
                    .as_ref()
                    .and_then(|i| i.title())
                    .unwrap_or_else(|| self.0.target.host.clone());
                *self.0.title.borrow_mut() = title.clone();
                (self.0.hooks.on_title)(&self.widget(), &title);

                let view = match self.build_view(&c.session, &c.page) {
                    Ok(view) => view,
                    Err(e) => {
                        self.show_error(&e);
                        return self.logout_in_background(c.session);
                    }
                };
                // A previous attempt (Retry after a failed load) may have left its widget.
                if let Some(old) = self.0.stack.child_by_name("console") {
                    self.0.stack.remove(&old);
                }
                let widget = view.widget();
                self.0.stack.add_named(&widget, "console");
                widget.show();
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

    fn build_view(&self, session: &bmc::Session, page: &bmc::ConsolePage) -> Result<ConsoleView> {
        let weak = self.downgrade();
        let on_close_requested = Box::new(move || {
            if let Some(tab) = upgrade(&weak) {
                tab.close();
            }
        });
        let weak = self.downgrade();
        let on_load_failed = Box::new(move |msg: String| {
            if let Some(tab) = upgrade(&weak) {
                tab.view_failed(msg);
            }
        });
        let hooks = webview::Hooks {
            on_close_requested,
            on_load_failed,
        };
        ConsoleView::new(page, session.host(), hooks)
    }

    fn show_error(&self, e: &anyhow::Error) {
        eprintln!("rustdrac: {}: {e:#}", self.0.target.host);
        *self.0.state.borrow_mut() = State::Failed;
        self.0.error.set_text(&format!("{e:#}"));
        self.0.stack.set_visible_child_name("error");
    }

    /// The view gave up on the page: log out so the BMC session is not left open, and
    /// offer Retry.
    fn view_failed(&self, msg: String) {
        let state = std::mem::replace(&mut *self.0.state.borrow_mut(), State::Failed);
        let State::Connected { session, .. } = state else {
            *self.0.state.borrow_mut() = state;
            return;
        };
        if let Some(view) = self.0.webview.borrow_mut().take() {
            view.close();
        }
        self.show_error(&anyhow::anyhow!(msg));
        self.logout_in_background(session);
    }

    fn logout_in_background(&self, session: bmc::Session) {
        let client = self.0.client.clone();
        gtk::gio::spawn_blocking(move || {
            if let Err(e) = bmc::logout(&client, &session) {
                eprintln!("rustdrac: {e:#}");
            }
        });
    }

    /// Disconnect the console and log out, then remove the tab.
    ///
    /// On iDRAC 8, just dropping the KVM WebSocket leaves a stale console session ("A session
    /// to this device already exists"), so disconnect the way the viewer's "Disconnect Viewer"
    /// button does. The iDRAC 9 and Supermicro viewers have no global hook; unloading the page
    /// closes their socket. Either way, give it a moment to reach the BMC, then log out.
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
                if let Some(view) = self.view() {
                    match disconnect_js {
                        Some(js) => view.evaluate_js(js, |_| {}),
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

    fn logout_and_finish(&self, session: bmc::Session) {
        let client = self.0.client.clone();
        let job = gtk::gio::spawn_blocking(move || bmc::logout(&client, &session));
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
            view.close();
        }
        (self.0.hooks.on_closed)(&self.widget());
    }
}

fn decode_png_data_url(url: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    let b64 = url.strip_prefix("data:image/png;base64,")?;
    base64::engine::general_purpose::STANDARD.decode(b64).ok()
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
    let session = bmc::login(client, &target.host, &target.user, &target.password)?;
    let identity = bmc::identity(
        client,
        &target.host,
        &target.user,
        &target.password,
        session.vendor(),
    );
    let title = identity
        .as_ref()
        .and_then(|i| i.title())
        .unwrap_or_else(|| target.host.clone());
    match bmc::console_page(client, &session, &title, LANG) {
        Ok(page) => Ok(Connected {
            session,
            identity,
            page,
        }),
        Err(e) => {
            let _ = bmc::logout(client, &session);
            Err(e)
        }
    }
}
