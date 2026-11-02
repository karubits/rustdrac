//! One BMC virtual console, shown as a notebook tab.

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::time::Duration;

use anyhow::Result;
use gtk::glib;
use gtk::prelude::*;
use reqwest::blocking::Client;

use crate::bmc;
use crate::idrac::HostHeaderRejected;
use crate::redfish::{self, Redfish};
use crate::theme;
use crate::webview::{self, ConsoleView};

/// The power menu: label, Redfish `ResetType`, and whether to ask first.
const POWER_ACTIONS: [(&str, &str, bool); 6] = [
    ("Power On", "On", false),
    ("Graceful Shutdown", "GracefulShutdown", true),
    ("Force Off", "ForceOff", true),
    ("Reset (force restart)", "ForceRestart", true),
    ("Graceful Restart", "GracefulRestart", true),
    ("Power Cycle", "PowerCycle", true),
];

/// How long after a power action to read the state back (the BMC lags a little).
const POWER_REFRESH: [Duration; 2] = [Duration::from_secs(3), Duration::from_secs(15)];

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
/// Asks for another user name and password for this target; `None` if cancelled.
pub type CredentialsHook = Box<dyn Fn(&Target) -> Option<(String, String)>>;
type PageHook = Box<dyn Fn(&gtk::Widget)>;
type TitleHook = Box<dyn Fn(&gtk::Widget, &str)>;

pub struct Hooks {
    /// Redfish told us the service tag / model (used to fill in the address book).
    pub on_identity: IdentityHook,
    /// The tab has disconnected and logged out; remove its page.
    pub on_closed: PageHook,
    /// The window title for this tab changed.
    pub on_title: TitleHook,
    /// The login was rejected and the user wants to enter other credentials.
    pub on_credentials: CredentialsHook,
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
    target: RefCell<Target>,
    client: Client,
    hooks: Hooks,
    state: RefCell<State>,
    title: RefCell<String>,
    stack: gtk::Stack,
    status: gtk::Label,
    error: gtk::Label,
    retry: gtk::Button,
    /// "Connect by IP address": shown when the BMC refused the host name.
    by_ip: gtk::Button,
    /// "Change Password…": shown when the login was rejected.
    password: gtk::Button,
    /// What that button connects to.
    ip: RefCell<Option<String>>,
    spinner: gtk::Spinner,
    tab_label: gtk::Label,
    tab_spinner: gtk::Spinner,
    tab: gtk::Box,
    webview: RefCell<Option<ConsoleView>>,
    service_tag: RefCell<String>,
    /// The page: the info strip above the stack.
    page: gtk::Box,
    model: Chip,
    serial: Chip,
    address: Chip,
    vendor: gtk::Label,
    /// Power state and actions (Redfish); shown once the server is identified.
    power: gtk::Button,
    power_menu: gtk::Menu,
    power_label: gtk::Label,
    power_items: Vec<gtk::MenuItem>,
    power_state: RefCell<redfish::Power>,
    redfish: RefCell<Option<Redfish>>,
}

#[derive(Clone)]
pub struct ConsoleTab(Rc<Inner>);

/// One "Model  PowerEdge R630" item of the info strip: a flat button whose click copies
/// the value and says "copied" in its place for a moment.
#[derive(Clone)]
struct Chip(Rc<ChipInner>);

struct ChipInner {
    button: gtk::Button,
    caption: gtk::Label,
    value: gtk::Label,
    /// Shows the value or "copied", without changing width.
    pages: gtk::Stack,
    text: RefCell<String>,
    flash: RefCell<Option<glib::SourceId>>,
}

const COPIED_FLASH: Duration = Duration::from_millis(1500);

impl Chip {
    fn new(caption: &str) -> Chip {
        let caption = gtk::Label::new(Some(caption));
        caption.style_context().add_class("caption");
        let value = gtk::Label::new(None);
        value.style_context().add_class("value");
        let copied = gtk::Label::new(Some("copied"));
        copied.style_context().add_class("caption");
        let pages = gtk::Stack::new();
        pages.set_homogeneous(true);
        pages.add_named(&value, "value");
        pages.add_named(&copied, "copied");
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        content.add(&caption);
        content.add(&pages);
        let button = gtk::Button::new();
        button.add(&content);
        button.set_relief(gtk::ReliefStyle::None);
        // The console keeps the keyboard.
        button.set_focus_on_click(false);
        button.set_tooltip_text(Some("Click to copy"));
        let chip = Chip(Rc::new(ChipInner {
            button,
            caption,
            value,
            pages,
            text: RefCell::new(String::new()),
            flash: RefCell::new(None),
        }));
        let weak = Rc::downgrade(&chip.0);
        chip.0.button.connect_clicked(move |_| {
            if let Some(inner) = weak.upgrade() {
                Chip(inner).copy();
            }
        });
        chip
    }

    fn button(&self) -> &gtk::Button {
        &self.0.button
    }

    fn text(&self) -> String {
        self.0.text.borrow().clone()
    }

    fn set_caption(&self, caption: &str) {
        self.0.caption.set_text(caption);
    }

    /// Shows the chip with `text`, or hides it when there is nothing to show.
    fn set(&self, text: &str) {
        *self.0.text.borrow_mut() = text.to_string();
        self.0.value.set_text(text);
        self.0.button.set_visible(!text.is_empty());
    }

    fn copy(&self) {
        let text = self.text();
        if text.is_empty() {
            return;
        }
        gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD).set_text(&text);
        self.flash_copied();
    }

    /// Says "copied" in place of the value for a moment.
    fn flash_copied(&self) {
        self.0.pages.set_visible_child_name("copied");
        if let Some(id) = self.0.flash.borrow_mut().take() {
            id.remove();
        }
        let weak = Rc::downgrade(&self.0);
        let id = glib::timeout_add_local_once(COPIED_FLASH, move || {
            if let Some(inner) = weak.upgrade() {
                inner.flash.borrow_mut().take();
                inner.pages.set_visible_child_name("value");
            }
        });
        *self.0.flash.borrow_mut() = Some(id);
    }
}

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
        let by_ip = gtk::Button::with_label("Connect by IP Address");
        by_ip.set_no_show_all(true);
        let password = gtk::Button::with_label("Change Password…");
        password.set_no_show_all(true);
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_halign(gtk::Align::Center);
        buttons.add(&retry);
        buttons.add(&by_ip);
        buttons.add(&password);
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

        // Info strip: model, serial and BMC address, each copied with a click.
        let model = Chip::new("Model");
        let serial = Chip::new("Serial");
        let address = Chip::new("BMC");
        address.set(&target.host);
        let copy_all = gtk::Button::from_icon_name(Some("edit-copy-symbolic"), gtk::IconSize::Menu);
        copy_all.set_relief(gtk::ReliefStyle::None);
        copy_all.set_focus_on_click(false);
        copy_all.set_tooltip_text(Some("Copy model, serial and address"));
        let vendor = gtk::Label::new(None);
        vendor.style_context().add_class("caption");

        // Power: "⏻ On ▾" with the reset actions in its menu.
        let power_label = gtk::Label::new(None);
        power_label.set_use_markup(true);
        let power_content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        power_content.add(&gtk::Image::from_icon_name(
            Some("system-shutdown-symbolic"),
            gtk::IconSize::Menu,
        ));
        power_content.add(&power_label);
        power_content.add(&gtk::Image::from_icon_name(
            Some("pan-down-symbolic"),
            gtk::IconSize::Menu,
        ));
        let power = gtk::Button::new();
        power.add(&power_content);
        // Hidden until the server is identified; its children stay shown.
        power_content.show_all();
        power.set_relief(gtk::ReliefStyle::None);
        power.set_focus_on_click(false);
        power.set_tooltip_text(Some("Server power (via Redfish)"));
        power.set_no_show_all(true);
        let power_menu = gtk::Menu::new();
        let power_items: Vec<gtk::MenuItem> = POWER_ACTIONS
            .iter()
            .map(|(label, _, _)| {
                let item = gtk::MenuItem::with_label(label);
                power_menu.append(&item);
                item
            })
            .collect();
        power_menu.show_all();

        let info = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        info.style_context().add_class("console-info");
        for chip in [&model, &serial, &address] {
            info.add(chip.button());
        }
        info.add(&copy_all);
        info.add(&gtk::Separator::new(gtk::Orientation::Vertical));
        info.add(&power);
        info.pack_end(&vendor, false, false, 6);
        let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        page.pack_start(&info, false, false, 0);
        page.pack_start(&stack, true, true, 0);

        let this = ConsoleTab(Rc::new(Inner {
            title: RefCell::new(target.name.clone()),
            target: RefCell::new(target),
            client,
            hooks,
            state: RefCell::new(State::Connecting),
            stack,
            status,
            error,
            retry: retry.clone(),
            by_ip,
            password,
            ip: RefCell::new(None),
            spinner,
            tab_label,
            tab_spinner,
            tab,
            webview: RefCell::new(None),
            service_tag: RefCell::new(String::new()),
            page,
            model,
            serial,
            address,
            vendor,
            power,
            power_menu,
            power_label,
            power_items,
            power_state: RefCell::new(redfish::Power::default()),
            redfish: RefCell::new(None),
        }));

        for (item, (label, reset_type, confirm)) in this.0.power_items.iter().zip(POWER_ACTIONS) {
            let weak = this.downgrade();
            item.connect_activate(move |_| {
                if let Some(tab) = upgrade(&weak) {
                    tab.power_action(label, reset_type, confirm);
                }
            });
        }
        // Open the menu below the button and re-read the state meanwhile.
        let weak = this.downgrade();
        this.0.power.connect_clicked(move |button| {
            if let Some(tab) = upgrade(&weak) {
                tab.refresh_power();
                tab.0.power_menu.popup_at_widget(
                    button,
                    gtk::gdk::Gravity::SouthWest,
                    gtk::gdk::Gravity::NorthWest,
                    None,
                );
            }
        });

        let weak = this.downgrade();
        copy_all.connect_clicked(move |_| {
            if let Some(tab) = upgrade(&weak) {
                tab.copy_all();
            }
        });
        let weak = this.downgrade();
        retry.connect_clicked(move |_| {
            if let Some(tab) = upgrade(&weak) {
                tab.connect();
            }
        });
        let weak = this.downgrade();
        this.0.by_ip.connect_clicked(move |_| {
            if let Some(tab) = upgrade(&weak) {
                tab.connect_by_ip();
            }
        });
        let weak = this.downgrade();
        this.0.password.connect_clicked(move |_| {
            if let Some(tab) = upgrade(&weak) {
                tab.change_credentials();
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

        this.0.page.show_all();
        // Unknown until Redfish answers.
        this.0.model.set("");
        this.0.serial.set("");
        this.connect();
        this
    }

    pub fn widget(&self) -> gtk::Widget {
        self.0.page.clone().upcast()
    }

    /// Fills the info strip from what Redfish said.
    fn show_identity(&self, identity: &bmc::Identity) {
        self.0.model.set(&identity.model);
        self.0.serial.set_caption(match identity.vendor {
            bmc::Vendor::Dell => "Service Tag",
            bmc::Vendor::Supermicro => "Serial",
        });
        self.0.serial.set(&identity.service_tag);
        self.0.address.set_caption(identity.vendor.bmc_name());
        self.0.vendor.set_text(identity.vendor.label());
        self.set_power(&identity.power);
        self.0.power.show();
    }

    fn set_power(&self, p: &redfish::Power) {
        let (color, text) = match p.state.as_str() {
            "On" => ("#3fb950", "On"),
            "Off" => ("#8b949e", "Off"),
            "" => ("#8b949e", "unknown"),
            other => ("#e3b341", other),
        };
        self.0
            .power_label
            .set_markup(&theme::dot_markup(color, text));
        for (item, (_, reset_type, _)) in self.0.power_items.iter().zip(POWER_ACTIONS) {
            let fits = match p.state.as_str() {
                "On" => reset_type != "On",
                "Off" => reset_type == "On",
                _ => true,
            };
            item.set_sensitive(fits && p.allows(reset_type));
        }
        *self.0.power_state.borrow_mut() = p.clone();
    }

    /// Reads the power state again in the background.
    fn refresh_power(&self) {
        let Some(redfish) = self.0.redfish.borrow().clone() else {
            return;
        };
        let job = gtk::gio::spawn_blocking(move || redfish.power());
        let weak = self.downgrade();
        glib::MainContext::default().spawn_local(async move {
            match job.await {
                Ok(Ok(p)) => {
                    if let Some(tab) = upgrade(&weak) {
                        tab.set_power(&p);
                    }
                }
                Ok(Err(e)) => eprintln!("rustdrac: {e:#}"),
                Err(_) => {}
            }
        });
    }

    /// A power menu item: ask first for anything but Power On, then act.
    fn power_action(&self, label: &'static str, reset_type: &'static str, confirm: bool) {
        if !confirm {
            return self.run_power(label, reset_type);
        }
        let target = self.target();
        let what = [self.0.model.text(), self.0.serial.text()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", ");
        let detail = match reset_type {
            "GracefulShutdown" => "The operating system is asked to shut down.",
            "GracefulRestart" => "The operating system is asked to restart.",
            "ForceOff" => "Power is cut without shutting the operating system down.",
            "ForceRestart" => "The server resets without shutting the operating system down.",
            "PowerCycle" => "Power is cut and restored without shutting the operating system down.",
            _ => "",
        };
        let dialog = gtk::MessageDialog::new(
            self.parent_window().as_ref(),
            gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
            gtk::MessageType::Question,
            gtk::ButtonsType::YesNo,
            &format!("{label}: {} ({what})?", target.name),
        );
        dialog.set_secondary_text(Some(
            format!("{detail} The console stays open.").trim_start(),
        ));
        dialog.set_title("Rust Drac");
        dialog.set_default_response(gtk::ResponseType::No);
        let weak = self.downgrade();
        dialog.connect_response(move |dialog, response| {
            // SAFETY: the dialog is not used after this point.
            unsafe { dialog.destroy() };
            if response == gtk::ResponseType::Yes
                && let Some(tab) = upgrade(&weak)
            {
                tab.run_power(label, reset_type);
            }
        });
        dialog.show();
    }

    fn run_power(&self, label: &'static str, reset_type: &'static str) {
        let Some(redfish) = self.0.redfish.borrow().clone() else {
            return;
        };
        self.0
            .power_label
            .set_markup(&theme::dot_markup("#e3b341", &format!("{label}…")));
        self.0.power.set_sensitive(false);
        let job = gtk::gio::spawn_blocking(move || redfish.reset(reset_type));
        let weak = self.downgrade();
        glib::MainContext::default().spawn_local(async move {
            let Some(tab) = upgrade(&weak) else {
                return;
            };
            tab.0.power.set_sensitive(true);
            match job.await {
                Ok(Ok(())) => {
                    for delay in POWER_REFRESH {
                        let weak = tab.downgrade();
                        glib::timeout_add_local_once(delay, move || {
                            if let Some(tab) = upgrade(&weak) {
                                tab.refresh_power();
                            }
                        });
                    }
                }
                Ok(Err(e)) => {
                    eprintln!("rustdrac: {e:#}");
                    tab.refresh_power();
                    crate::manager::error_dialog(tab.parent_window().as_ref(), &format!("{e:#}"));
                }
                Err(_) => tab.refresh_power(),
            }
        });
    }

    /// "PowerEdge R630, 76G2F42, 10.2.31.211" (whatever is known) to the clipboard.
    fn copy_all(&self) {
        let parts: Vec<String> = [&self.0.model, &self.0.serial, &self.0.address]
            .iter()
            .map(|c| c.text())
            .filter(|t| !t.is_empty())
            .collect();
        gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD).set_text(&parts.join(", "));
        self.0.address.flash_copied();
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

    pub fn target(&self) -> Target {
        self.0.target.borrow().clone()
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
        let target = self.target();
        *self.0.state.borrow_mut() = State::Connecting;
        self.0.status.set_text(&format!(
            "Connecting to {} as {}…",
            target.host, target.user
        ));
        self.0.stack.set_visible_child_name("connecting");
        self.set_busy(true);

        let client = self.0.client.clone();
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
                let target = self.target();
                *self.0.redfish.borrow_mut() = Some(
                    Redfish::new(&self.0.client, &target.host, &target.user, &target.password)
                        .with_vendor(Some(c.session.vendor())),
                );
                if let Some(identity) = &c.identity {
                    *self.0.service_tag.borrow_mut() = identity.service_tag.clone();
                    self.show_identity(identity);
                    (self.0.hooks.on_identity)(&self.target(), identity);
                }
                let title = c
                    .identity
                    .as_ref()
                    .and_then(|i| i.title())
                    .unwrap_or_else(|| self.target().host);
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
        let host = self.target().host;
        eprintln!("rustdrac: {host}: {e:#}");
        *self.0.state.borrow_mut() = State::Failed;
        self.0.by_ip.hide();
        self.0
            .by_ip
            .style_context()
            .remove_class("suggested-action");
        self.0.retry.style_context().add_class("suggested-action");
        self.0.password.set_visible(bmc::is_auth_failure(e));
        if bmc::is_auth_failure(e) {
            // Another password is the likely fix, not Retry.
            self.0
                .retry
                .style_context()
                .remove_class("suggested-action");
            self.0
                .password
                .style_context()
                .add_class("suggested-action");
        } else {
            self.0
                .password
                .style_context()
                .remove_class("suggested-action");
        }
        self.0.stack.set_visible_child_name("error");
        let rejected = e
            .chain()
            .find_map(|c| c.downcast_ref::<HostHeaderRejected>());
        let Some(rejected) = rejected else {
            self.0.error.set_justify(gtk::Justification::Center);
            self.0.error.style_context().add_class("error-message");
            self.0.error.set_text(&format!("{e:#}"));
            return;
        };
        // Prose, with only the commands in monospace.
        self.0.error.set_justify(gtk::Justification::Left);
        self.0.error.style_context().remove_class("error-message");
        self.0.error.set_markup(&host_header_markup(rejected));
        if split_port(&host).0.parse::<std::net::IpAddr>().is_ok() {
            return;
        }
        // Offer the IP address instead: resolve the name off the main loop first.
        let name = host.clone();
        let job = gtk::gio::spawn_blocking(move || resolve(&name));
        let weak = self.downgrade();
        glib::MainContext::default().spawn_local(async move {
            let ip = job.await.unwrap_or(None);
            let Some(tab) = upgrade(&weak) else {
                return;
            };
            if tab.target().host != host || !matches!(*tab.0.state.borrow(), State::Failed) {
                return;
            }
            match ip {
                Some(ip) => tab.offer_ip(&ip),
                None => tab.0.error.set_markup(&format!(
                    "{}\n\n<b>{} does not resolve here, so there is no IP address to try.</b>",
                    host_header_markup(&HostHeaderRejected { host: host.clone() }),
                    glib::markup_escape_text(&host)
                )),
            }
        });
    }

    /// The host name resolved: offer it on the error page (next to the fixes, which stay
    /// readable and copyable).
    fn offer_ip(&self, ip: &str) {
        *self.0.ip.borrow_mut() = Some(ip.to_string());
        self.0
            .by_ip
            .set_label(&format!("Connect by IP Address ({ip})"));
        // The IP address is the likely choice now, not Retry.
        self.0
            .retry
            .style_context()
            .remove_class("suggested-action");
        self.0.by_ip.style_context().add_class("suggested-action");
        self.0.by_ip.show();
    }

    /// Retry with the IP address the button names (the tab keeps its name).
    fn connect_by_ip(&self) {
        let Some(ip) = self.0.ip.borrow_mut().take() else {
            return;
        };
        self.0.address.set(&ip);
        self.0.target.borrow_mut().host = ip;
        self.0.by_ip.hide();
        self.connect();
    }

    /// Ask for other credentials and connect again with them.
    fn change_credentials(&self) {
        let Some((user, password)) = (self.0.hooks.on_credentials)(&self.target()) else {
            return;
        };
        {
            let mut target = self.0.target.borrow_mut();
            target.user = user;
            target.password = password;
        }
        self.connect();
    }

    fn parent_window(&self) -> Option<gtk::Window> {
        self.0.page.toplevel().and_then(|w| w.downcast().ok())
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
                    .set_text(&format!("{} (closing)", self.target().name));
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

/// The host header refusal as a readable error page: why, then the fixes with their
/// commands in monospace.
pub(crate) fn host_header_markup(rejected: &HostHeaderRejected) -> String {
    let esc = glib::markup_escape_text;
    let mut out = format!(
        "<b>{} answered \"Access Error: 400 -- Bad Request\".</b>\n\n{}",
        esc(&rejected.host),
        esc(&rejected.why())
    );
    for (what, commands) in rejected.fixes() {
        out += &format!("\n\n{}", esc(&what));
        for c in commands {
            out += &format!("\n<tt>  {}</tt>", esc(&c));
        }
    }
    out
}

/// "host:port" → ("host", Some("port")); anything else is all host.
pub(crate) fn split_port(host: &str) -> (&str, Option<&str>) {
    match host.rsplit_once(':') {
        Some((name, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
            (name, Some(port))
        }
        _ => (host, None),
    }
}

/// The first IPv4 address (else IPv6) `host` resolves to, as text, keeping any port.
pub(crate) fn resolve(host: &str) -> Option<String> {
    use std::net::ToSocketAddrs;
    let (name, port) = split_port(host);
    let addrs: Vec<_> = (name, 443).to_socket_addrs().ok()?.collect();
    let ip = addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or(addrs.first())
        .map(|a| a.ip())?;
    Some(match (ip, port) {
        (std::net::IpAddr::V6(v6), Some(p)) => format!("[{v6}]:{p}"),
        (ip, Some(p)) => format!("{ip}:{p}"),
        (ip, None) => ip.to_string(),
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_are_split_off_and_kept() {
        assert_eq!(split_port("bmc.example.com"), ("bmc.example.com", None));
        assert_eq!(
            split_port("bmc.example.com:8443"),
            ("bmc.example.com", Some("8443"))
        );
        assert_eq!(split_port("10.0.0.1:x"), ("10.0.0.1:x", None));
        assert_eq!(resolve("localhost:8443").as_deref(), Some("127.0.0.1:8443"));
        assert_eq!(resolve("localhost").as_deref(), Some("127.0.0.1"));
    }
}
