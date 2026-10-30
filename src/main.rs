mod console;
mod export;
mod health;
mod idrac;
mod manager;
mod redfish;
mod store;
mod theme;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use clap::Parser;
use gtk::glib;
use gtk::prelude::*;
use reqwest::blocking::Client;
use webkit2gtk::{SnapshotOptions, SnapshotRegion, WebViewExt};

use console::{ConsoleTab, Target};
use health::HealthTab;
use manager::Check;
use manager::{Manager, error_dialog};
use redfish::{Health, Redfish};
use store::{Connection, Store};

const APP_TITLE: &str = "Rust Drac";

/// Dell iDRAC 8/9 HTML5 virtual console, without a browser.
///
/// With no arguments the connection manager opens. Each HOST opens a console tab; a HOST can
/// be an address, or the name, address or service tag of a saved connection.
#[derive(Parser, Debug)]
#[command(version)]
struct Cli {
    /// iDRAC hostnames/IPs or saved connection names to open right away.
    hosts: Vec<String>,
    /// iDRAC user (overrides the saved connection)
    #[arg(short, long)]
    user: Option<String>,
    /// iDRAC password (overrides the saved one)
    #[arg(short, long, env = "RUSTDRAC_PASSWORD", hide_env_values = true)]
    password: Option<String>,
    /// Open the Health & SEL view for each HOST instead of its console
    #[arg(long)]
    health: bool,
    /// Colour theme (default: `defaults.theme` from connections.yaml, else dark)
    #[arg(long, value_enum)]
    theme: Option<theme::Theme>,
}

fn main() {
    use_software_gl_without_gpu();
    let cli = Cli::parse();
    // Matches the .desktop file's StartupWMClass so docks group the window with its launcher.
    glib::set_prgname(Some("rustdrac"));
    glib::set_application_name("Rust Drac");
    if let Err(e) = gtk::init() {
        eprintln!("rustdrac: cannot initialise GTK: {e}");
        std::process::exit(1);
    }
    set_default_icon();
    if let Err(e) = run(cli) {
        eprintln!("rustdrac: {e:#}");
        error_dialog(None, &format!("{e:#}"));
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let store = Store::load()?;
    let configured = store.defaults.theme.as_deref().and_then(|t| {
        let parsed = theme::Theme::parse(t);
        if parsed.is_none() {
            eprintln!("rustdrac: unknown theme {t:?}; use dark, light or system");
        }
        parsed
    });
    theme::apply(cli.theme.or(configured).unwrap_or(theme::Theme::Dark));
    let app = App::new(store, idrac::http_client()?);
    for host in &cli.hosts {
        app.open_from_cli(
            host,
            cli.user.as_deref(),
            cli.password.as_deref(),
            cli.health,
        );
    }
    app.schedule_checks();
    app.testing_aids();
    gtk::main();
    Ok(())
}

struct AppInner {
    window: gtk::Window,
    header: gtk::HeaderBar,
    /// Header-bar actions on the current tab's server (shown for console / health tabs).
    screenshot_button: gtk::Button,
    export_button: gtk::Button,
    toast: Toast,
    notebook: gtk::Notebook,
    manager: RefCell<Option<Manager>>,
    store: Rc<RefCell<Store>>,
    client: Client,
    tabs: RefCell<Vec<ConsoleTab>>,
    health_tabs: RefCell<Vec<HealthTab>>,
    quitting: Cell<bool>,
    checks_running: Cell<usize>,
    last_health: RefCell<HashMap<String, Option<Health>>>,
    /// Passwords typed (or given on the command line) this run, by connection id, so alarm
    /// checks can use them too. Memory only.
    session_passwords: RefCell<HashMap<String, String>>,
}

#[derive(Clone)]
struct App(Rc<AppInner>);

impl App {
    fn new(store: Store, client: Client) -> App {
        let window = gtk::Window::new(gtk::WindowType::Toplevel);
        window.set_title(APP_TITLE);
        window.set_default_size(1280, 860);
        let header = gtk::HeaderBar::builder()
            .title(APP_TITLE)
            .show_close_button(true)
            .build();
        window.set_titlebar(Some(&header));
        let screenshot_button = header_button(
            "Screenshot",
            "camera-photo-symbolic",
            "Save the console picture as a PNG",
        );
        let export_button = header_button(
            "Export Logs",
            "document-save-symbolic",
            "Save a zip with a hardware summary, health, the SEL and the Lifecycle log",
        );
        header.pack_end(&export_button);
        header.pack_end(&screenshot_button);
        let notebook = gtk::Notebook::builder()
            .scrollable(true)
            .show_border(false)
            .build();
        let toast = Toast::new();
        let overlay = gtk::Overlay::new();
        overlay.add(&notebook);
        overlay.add_overlay(&toast.revealer);
        window.add(&overlay);

        let app = App(Rc::new(AppInner {
            window,
            header,
            screenshot_button,
            export_button,
            toast,
            notebook,
            manager: RefCell::new(None),
            store: Rc::new(RefCell::new(store)),
            client,
            tabs: RefCell::new(Vec::new()),
            health_tabs: RefCell::new(Vec::new()),
            quitting: Cell::new(false),
            checks_running: Cell::new(0),
            last_health: RefCell::new(HashMap::new()),
            session_passwords: RefCell::new(HashMap::new()),
        }));

        let weak = app.downgrade();
        let weak2 = app.downgrade();
        let (weak3, weak4, weak5) = (app.downgrade(), app.downgrade(), app.downgrade());
        let manager = Manager::new(
            app.0.store.clone(),
            manager::Hooks {
                connect: Box::new(move |c| {
                    if let Some(app) = upgrade(&weak) {
                        app.connect_saved(c.clone(), None, None);
                    }
                }),
                quick_connect: Box::new(move || {
                    if let Some(app) = upgrade(&weak2) {
                        app.quick_connect("");
                    }
                }),
                health: Box::new(move |c| {
                    if let Some(app) = upgrade(&weak3) {
                        app.open_health(c.clone(), None, None);
                    }
                }),
                check_all: Box::new(move || {
                    if let Some(app) = upgrade(&weak4) {
                        app.check_all();
                    }
                }),
                export: Box::new(move |c| {
                    if let Some(app) = upgrade(&weak5) {
                        app.export_saved(c.clone());
                    }
                }),
            },
        );
        let label = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        label.add(&gtk::Image::from_icon_name(
            Some("view-list-symbolic"),
            gtk::IconSize::Menu,
        ));
        label.add(&gtk::Label::new(Some("Connections")));
        label.show_all();
        app.0.notebook.append_page(&manager.widget(), Some(&label));
        *app.0.manager.borrow_mut() = Some(manager);

        let weak = app.downgrade();
        app.0.notebook.connect_switch_page(move |_, page, _| {
            let Some(app) = upgrade(&weak) else { return };
            let tab = app.tab_for(page);
            let health = app.health_for(page);
            let title = tab
                .as_ref()
                .map(|t| t.title())
                .or_else(|| health.as_ref().map(|h| h.title()));
            app.set_subtitle(title.as_deref());
            app.0.screenshot_button.set_visible(tab.is_some());
            app.0
                .export_button
                .set_visible(tab.is_some() || health.is_some());
            // Focus after the switch has finished.
            let weak = app.downgrade();
            glib::idle_add_local_once(move || {
                let Some(app) = upgrade(&weak) else { return };
                match (tab, health) {
                    (Some(t), _) => t.focus(),
                    (None, Some(_)) => {}
                    (None, None) => app.manager().focus_search(),
                }
            });
        });

        // Closing the window disconnects every console first (see ConsoleTab::close).
        let weak = app.downgrade();
        app.0.window.connect_delete_event(move |_, _| {
            let Some(app) = upgrade(&weak) else {
                return glib::Propagation::Proceed;
            };
            let tabs = app.0.tabs.borrow().clone();
            if tabs.is_empty() {
                gtk::main_quit();
                return glib::Propagation::Proceed;
            }
            app.0.quitting.set(true);
            app.set_subtitle(Some("Disconnecting…"));
            for tab in tabs {
                tab.close();
            }
            glib::Propagation::Stop
        });

        let weak = app.downgrade();
        app.0.screenshot_button.connect_clicked(move |_| {
            if let Some(app) = upgrade(&weak) {
                app.take_screenshot();
            }
        });
        let weak = app.downgrade();
        app.0.export_button.connect_clicked(move |_| {
            if let Some(app) = upgrade(&weak) {
                app.export_current();
            }
        });

        app.0.window.show_all();
        app.0.toast.revealer.set_reveal_child(false);
        app.0.screenshot_button.hide();
        app.0.export_button.hide();
        app.manager().focus_search();
        app
    }

    fn current_page(&self) -> Option<gtk::Widget> {
        self.0.notebook.nth_page(self.0.notebook.current_page())
    }

    /// Save the current console as `<screenshot dir>/<name>-<tag>-<time>.png`.
    fn take_screenshot(&self) {
        let Some(tab) = self.current_page().and_then(|p| self.tab_for(&p)) else {
            return;
        };
        let dir = self.0.store.borrow().screenshot_dir();
        let base = export::base_name(&tab.target().name, &tab.service_tag(), &now_stamp());
        let weak = self.downgrade();
        tab.screenshot(move |res| {
            let Some(app) = upgrade(&weak) else { return };
            let saved = res.and_then(|png| {
                std::fs::create_dir_all(&dir)?;
                let path = dir.join(format!("{base}.png"));
                std::fs::write(&path, png)?;
                Ok(path)
            });
            match saved {
                Ok(path) => app.0.toast.show(
                    &format!("Screenshot saved: {}", store::display_path(&path)),
                    Some(dir),
                ),
                Err(e) => app.0.toast.show(&format!("Screenshot failed: {e:#}"), None),
            }
        });
    }

    /// Export logs for the server of the current console or health tab.
    fn export_current(&self) {
        let Some(page) = self.current_page() else {
            return;
        };
        if let Some(tab) = self.tab_for(&page) {
            let t = tab.target();
            let server = export_server(&t.name, &t.host, &t.user, &t.password, &tab.service_tag());
            self.export_logs(server);
        } else if let Some(tab) = self.health_for(&page) {
            let t = tab.target();
            let tag = t
                .connection_id
                .as_ref()
                .and_then(|id| self.0.store.borrow().get(id).map(|c| c.service_tag.clone()))
                .unwrap_or_default();
            self.export_logs(export_server(&t.name, &t.host, &t.user, &t.password, &tag));
        }
    }

    fn export_saved(&self, conn: Connection) {
        let Some((user, password)) = self.credentials(conn.clone(), None, None) else {
            return;
        };
        self.export_logs(export_server(
            conn.display_name(),
            &conn.address,
            &user,
            &password,
            &conn.service_tag,
        ));
    }

    /// Ask where to save, then read everything in the background behind a progress dialog.
    fn export_logs(&self, server: ExportServer) {
        let stamp = now_stamp();
        let base = export::base_name(&server.name, &server.service_tag, &stamp);
        let chooser = gtk::FileChooserNative::new(
            Some("Export Logs"),
            Some(&self.0.window),
            gtk::FileChooserAction::Save,
            Some("Export"),
            Some("Cancel"),
        );
        chooser.set_do_overwrite_confirmation(true);
        chooser.set_current_name(&format!("{base}.zip"));
        if let Some(dir) = dirs::download_dir() {
            chooser.set_current_folder(dir);
        }
        if chooser.run() != gtk::ResponseType::Accept {
            return;
        }
        let Some(path) = chooser.filename() else {
            return;
        };
        let folder = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or(base);

        let dialog = gtk::Dialog::with_buttons(
            Some("Export Logs"),
            Some(&self.0.window),
            gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
            &[("Cancel", gtk::ResponseType::Cancel)],
        );
        dialog.set_default_size(420, -1);
        let label = gtk::Label::new(Some(&format!("Reading {} from {}…", "system", server.name)));
        label.set_xalign(0.0);
        let bar = gtk::ProgressBar::new();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
        content.set_border_width(16);
        content.add(&label);
        content.add(&bar);
        dialog.content_area().add(&content);
        dialog.show_all();

        let progress = Arc::new(Mutex::new(String::from("system")));
        let cancel = Arc::new(AtomicBool::new(false));
        let c = cancel.clone();
        let l = label.clone();
        dialog.connect_response(move |_, _| {
            c.store(true, Ordering::Relaxed);
            l.set_text("Cancelling after the current request…");
        });
        let (p, l, b, name) = (
            progress.clone(),
            label.clone(),
            bar.clone(),
            server.name.clone(),
        );
        let ticker = glib::timeout_add_local(std::time::Duration::from_millis(200), move || {
            b.pulse();
            let step = p.lock().map(|s| s.clone()).unwrap_or_default();
            if !step.is_empty() {
                l.set_text(&format!("Reading {step} from {name}…"));
            }
            glib::ControlFlow::Continue
        });

        let client = self.0.client.clone();
        let out = path.clone();
        let job = gtk::gio::spawn_blocking(move || -> Result<()> {
            let rf = Redfish::new(&client, &server.host, &server.user, &server.password);
            let collected = export::collect(
                &rf,
                &|step| {
                    if let Ok(mut p) = progress.lock() {
                        *p = step.to_string();
                    }
                },
                &|| cancel.load(Ordering::Relaxed),
            )?;
            if let Ok(mut p) = progress.lock() {
                p.clear();
            }
            let summary = export::summary_json(&collected, &server.name, &server.host, &now_iso());
            let zip = export::build_zip(&collected, &folder, &summary)?;
            std::fs::write(&out, zip)?;
            Ok(())
        });
        let weak = self.downgrade();
        glib::MainContext::default().spawn_local(async move {
            let result = job
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("export task panicked")));
            ticker.remove();
            // SAFETY: the dialog is not used after this point.
            unsafe { dialog.destroy() };
            let Some(app) = upgrade(&weak) else { return };
            match result {
                Ok(()) => app.0.toast.show(
                    &format!("Logs exported: {}", store::display_path(&path)),
                    path.parent().map(PathBuf::from),
                ),
                Err(e) if e.to_string() == "cancelled" => {
                    app.0.toast.show("Export cancelled", None)
                }
                Err(e) => error_dialog(Some(&app.0.window), &format!("Export failed: {e:#}")),
            }
        });
    }

    /// Show what the current tab is connected to in the header bar and window title.
    fn set_subtitle(&self, subtitle: Option<&str>) {
        self.0.window.set_title(&match subtitle {
            Some(s) => format!("{s} — {APP_TITLE}"),
            None => APP_TITLE.to_string(),
        });
        // GtkWindow copies its title into the header bar; keep the header bar's own.
        self.0.header.set_title(Some(APP_TITLE));
        self.0.header.set_subtitle(subtitle);
    }

    fn downgrade(&self) -> Weak<AppInner> {
        Rc::downgrade(&self.0)
    }

    fn manager(&self) -> Manager {
        self.0
            .manager
            .borrow()
            .clone()
            .expect("manager is created in App::new")
    }

    fn tab_for(&self, page: &gtk::Widget) -> Option<ConsoleTab> {
        self.0
            .tabs
            .borrow()
            .iter()
            .find(|t| &t.widget() == page)
            .cloned()
    }

    fn open_from_cli(&self, host: &str, user: Option<&str>, password: Option<&str>, health: bool) {
        let saved = self.0.store.borrow().find(host).cloned();
        if health {
            // Health views need an address-book entry to report to; use a temporary one.
            let conn = saved.unwrap_or_else(|| Connection {
                id: store::new_id(),
                address: host.to_string(),
                ..Default::default()
            });
            return self.open_health(conn, user.map(str::to_string), password.map(str::to_string));
        }
        match saved {
            Some(c) => {
                self.connect_saved(c, user.map(str::to_string), password.map(str::to_string))
            }
            None => match password {
                Some(p) => self.open(Target {
                    name: host.to_string(),
                    host: host.to_string(),
                    user: user
                        .map(str::to_string)
                        .unwrap_or_else(|| self.0.store.borrow().default_user()),
                    password: p.to_string(),
                    connection_id: None,
                }),
                None => self.quick_connect(host),
            },
        }
    }

    /// Open a saved connection, asking for the password if it is not saved.
    fn connect_saved(&self, conn: Connection, user: Option<String>, password: Option<String>) {
        let Some((user, password)) = self.credentials(conn.clone(), user, password) else {
            return;
        };
        self.open(Target {
            name: conn.display_name().to_string(),
            host: conn.address.clone(),
            user,
            password,
            connection_id: Some(conn.id.clone()),
        });
    }

    fn user_for(&self, conn: &Connection) -> String {
        if conn.user.is_empty() {
            self.0.store.borrow().default_user()
        } else {
            conn.user.clone()
        }
    }

    /// User and password for a saved connection: given, saved, or asked for (and optionally
    /// saved). `None` if the user cancelled.
    fn credentials(
        &self,
        mut conn: Connection,
        user: Option<String>,
        password: Option<String>,
    ) -> Option<(String, String)> {
        let user = user.unwrap_or_else(|| self.user_for(&conn));
        let password = match password.or_else(|| conn.saved_password()) {
            Some(p) => p,
            None => {
                let (p, remember) = password_dialog(&self.0.window, &conn, &user)?;
                if remember {
                    conn.save_password = true;
                    let saved = conn.set_saved_password(&p).and_then(|()| {
                        let mut store = self.0.store.borrow_mut();
                        store.upsert(conn.clone());
                        store.save()
                    });
                    if let Err(e) = saved {
                        error_dialog(Some(&self.0.window), &format!("{e:#}"));
                    }
                    self.manager().refresh();
                }
                p
            }
        };
        self.0
            .session_passwords
            .borrow_mut()
            .insert(conn.id.clone(), password.clone());
        Some((user, password))
    }

    fn health_for(&self, page: &gtk::Widget) -> Option<HealthTab> {
        self.0
            .health_tabs
            .borrow()
            .iter()
            .find(|t| &t.widget() == page)
            .cloned()
    }

    /// Open (or switch to) the Health & SEL tab of a saved connection.
    fn open_health(&self, conn: Connection, user: Option<String>, password: Option<String>) {
        let existing = self
            .0
            .health_tabs
            .borrow()
            .iter()
            .find(|t| t.connection_id() == Some(conn.id.as_str()))
            .cloned();
        if let Some(tab) = existing {
            if let Some(n) = self.0.notebook.page_num(&tab.widget()) {
                self.0.notebook.set_current_page(Some(n));
            }
            tab.refresh();
            return;
        }
        let Some((user, password)) = self.credentials(conn.clone(), user, password) else {
            return;
        };
        let (w1, w2) = (self.downgrade(), self.downgrade());
        let tab = HealthTab::open(
            health::Target {
                name: conn.display_name().to_string(),
                host: conn.address.clone(),
                user,
                password,
                connection_id: Some(conn.id.clone()),
            },
            self.0.client.clone(),
            health::Hooks {
                on_report: Box::new(move |target, result| {
                    let (Some(app), Some(id)) = (upgrade(&w1), &target.connection_id) else {
                        return;
                    };
                    let check = match result {
                        Ok(report) => Check::Done(report.clone()),
                        Err(e) => Check::Failed(e.clone()),
                    };
                    app.manager().set_check(id, check);
                }),
                on_closed: Box::new(move |page| {
                    let Some(app) = upgrade(&w2) else { return };
                    if let Some(n) = app.0.notebook.page_num(page) {
                        app.0.notebook.remove_page(Some(n));
                    }
                    app.0
                        .health_tabs
                        .borrow_mut()
                        .retain(|t| &t.widget() != page);
                }),
            },
        );
        self.0.health_tabs.borrow_mut().push(tab.clone());
        let page = tab.widget();
        let n = self.0.notebook.append_page(&page, Some(&tab.tab_widget()));
        self.0.notebook.set_tab_reorderable(&page, true);
        self.0.notebook.set_current_page(Some(n));
    }

    /// Alarm check of every saved connection that has a saved password. Never prompts.
    fn check_all(&self) {
        if self.0.checks_running.get() > 0 {
            return;
        }
        let connections = self.0.store.borrow().connections.clone();
        let manager = self.manager();
        for conn in connections {
            let session = self.0.session_passwords.borrow().get(&conn.id).cloned();
            let Some(password) = session.or_else(|| conn.saved_password()) else {
                manager.set_check(&conn.id, Check::NoPassword);
                continue;
            };
            let user = self.user_for(&conn);
            manager.set_check(&conn.id, Check::Checking);
            self.0.checks_running.set(self.0.checks_running.get() + 1);
            let client = self.0.client.clone();
            let host = conn.address.clone();
            let job = gtk::gio::spawn_blocking(move || {
                Redfish::new(&client, &host, &user, &password).check()
            });
            let weak = self.downgrade();
            glib::MainContext::default().spawn_local(async move {
                let result = job
                    .await
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("check task panicked")));
                let Some(app) = upgrade(&weak) else { return };
                app.check_done(&conn, result);
            });
        }
        if self.0.checks_running.get() == 0 {
            manager.checked_at(now_hms());
        }
    }

    fn check_done(&self, conn: &Connection, result: Result<redfish::HealthReport>) {
        let check = match result {
            Ok(report) => Check::Done(report),
            Err(e) => Check::Failed(format!("{e:#}")),
        };
        let new = check.health();
        self.manager().set_check(&conn.id, check.clone());
        // Notify when a server gets worse than at its last successful check.
        let before = self.0.last_health.borrow_mut().insert(conn.id.clone(), new);
        if let (Some(h), Check::Done(report)) = (new, &check)
            && h.is_alarm()
            && before.flatten().is_none_or(|b| b < h)
        {
            notify(
                &format!("{}: {}", conn.display_name(), h.label()),
                &report.summary(),
                h,
            );
        }
        let left = self.0.checks_running.get().saturating_sub(1);
        self.0.checks_running.set(left);
        if left == 0 {
            self.manager().checked_at(now_hms());
        }
    }

    /// Re-run the alarm check every `defaults.health_check_minutes` (0 = never).
    fn schedule_checks(&self) {
        let minutes = self.0.store.borrow().health_check_minutes();
        if minutes == 0 {
            return;
        }
        // First round shortly after start-up, then periodically.
        let weak = self.downgrade();
        glib::timeout_add_seconds_local_once(2, move || {
            if let Some(app) = upgrade(&weak) {
                app.check_all();
            }
        });
        let weak = self.downgrade();
        let secs = u32::try_from(minutes.saturating_mul(60)).unwrap_or(u32::MAX);
        glib::timeout_add_seconds_local(secs, move || match upgrade(&weak) {
            Some(app) => {
                app.check_all();
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });
    }

    /// Ad-hoc connection, optionally added to the address book.
    fn quick_connect(&self, host: &str) {
        let default_user = self.0.store.borrow().default_user();
        let Some(q) = quick_connect_dialog(&self.0.window, host, &default_user) else {
            return;
        };
        let mut connection_id = None;
        if q.add_to_book {
            let conn = Connection {
                id: store::new_id(),
                address: q.host.clone(),
                user: q.user.clone(),
                save_password: q.save_password,
                ..Default::default()
            };
            let saved = (|| {
                if q.save_password {
                    conn.set_saved_password(&q.password)?;
                }
                let mut store = self.0.store.borrow_mut();
                store.upsert(conn.clone());
                store.save()
            })();
            if let Err(e) = saved {
                error_dialog(Some(&self.0.window), &format!("{e:#}"));
            }
            self.manager().refresh();
            connection_id = Some(conn.id);
        }
        self.open(Target {
            name: q.host.clone(),
            host: q.host,
            user: q.user,
            password: q.password,
            connection_id,
        });
    }

    fn open(&self, target: Target) {
        let (w1, w2, w3) = (self.downgrade(), self.downgrade(), self.downgrade());
        let tab = ConsoleTab::open(
            target,
            self.0.client.clone(),
            console::Hooks {
                on_identity: Box::new(move |target, identity| {
                    if let Some(app) = upgrade(&w1) {
                        app.learn_identity(target, identity);
                    }
                }),
                on_closed: Box::new(move |page| {
                    if let Some(app) = upgrade(&w2) {
                        app.tab_closed(page);
                    }
                }),
                on_title: Box::new(move |page, title| {
                    let Some(app) = upgrade(&w3) else { return };
                    if app.0.notebook.current_page() == app.0.notebook.page_num(page) {
                        app.set_subtitle(Some(title));
                    }
                    if let Some(label) = app.0.notebook.tab_label(page) {
                        label.set_tooltip_text(Some(title));
                    }
                }),
            },
        );
        self.0.tabs.borrow_mut().push(tab.clone());
        let page = tab.widget();
        let n = self.0.notebook.append_page(&page, Some(&tab.tab_widget()));
        self.0.notebook.set_tab_reorderable(&page, true);
        self.0.notebook.set_current_page(Some(n));
    }

    fn tab_closed(&self, page: &gtk::Widget) {
        if let Some(n) = self.0.notebook.page_num(page) {
            self.0.notebook.remove_page(Some(n));
        }
        self.0.tabs.borrow_mut().retain(|t| !t.is_closed());
        if self.0.quitting.get() && self.0.tabs.borrow().is_empty() {
            gtk::main_quit();
        }
    }

    /// Fill in the service tag / model of a saved connection from what Redfish reported.
    fn learn_identity(&self, target: &Target, identity: &idrac::Identity) {
        let Some(id) = &target.connection_id else {
            return;
        };
        let mut store = self.0.store.borrow_mut();
        let Some(conn) = store.get_mut(id) else {
            return;
        };
        let mut changed = false;
        for (field, value) in [
            (&mut conn.service_tag, &identity.service_tag),
            (&mut conn.model, &identity.model),
        ] {
            if !value.is_empty() && field != value {
                *field = value.clone();
                changed = true;
            }
        }
        if !changed {
            return;
        }
        let saved = store.save();
        drop(store);
        if let Err(e) = saved {
            eprintln!("rustdrac: {e:#}");
        }
        self.manager().refresh();
    }

    /// `RUSTDRAC_AUTOCLOSE=<secs>` closes the window after N seconds (disconnecting every
    /// console); with `RUSTDRAC_SNAPSHOT=<file.png>` the current tab is saved first.
    fn testing_aids(&self) {
        let Some(secs) = std::env::var("RUSTDRAC_AUTOCLOSE")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            return;
        };
        let snapshot = std::env::var_os("RUSTDRAC_SNAPSHOT");
        // RUSTDRAC_TEST_SCREENSHOT=<secs> presses the screenshot button after N seconds.
        if let Some(at) = std::env::var("RUSTDRAC_TEST_SCREENSHOT")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
        {
            let weak = self.downgrade();
            glib::timeout_add_seconds_local_once(at, move || {
                if let Some(app) = upgrade(&weak) {
                    app.take_screenshot();
                }
            });
        }
        // RUSTDRAC_SNAPSHOT_PAGE=<n> switches to tab n a second before the snapshot.
        if let Some(n) = std::env::var("RUSTDRAC_SNAPSHOT_PAGE")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
        {
            let weak = self.downgrade();
            glib::timeout_add_seconds_local_once(secs.saturating_sub(1), move || {
                if let Some(app) = upgrade(&weak) {
                    app.0.notebook.set_current_page(Some(n));
                }
            });
        }
        let weak = self.downgrade();
        glib::timeout_add_seconds_local_once(secs, move || {
            let Some(app) = upgrade(&weak) else { return };
            let window = app.0.window.clone();
            let Some(path) = snapshot else {
                return window.close();
            };

            let page = app.0.notebook.nth_page(app.0.notebook.current_page());
            let view = page.and_then(|p| app.tab_for(&p)).and_then(|t| t.webview());
            match view {
                Some(view) => view.snapshot(
                    SnapshotRegion::Visible,
                    SnapshotOptions::NONE,
                    None::<&gtk::gio::Cancellable>,
                    move |res| {
                        let saved = res
                            .map_err(anyhow::Error::from)
                            .and_then(|surface| save_png(surface, &path));
                        report_snapshot(saved);
                        window.close();
                    },
                ),
                None => {
                    report_snapshot(render_window(&window).and_then(|s| save_png(s, &path)));
                    window.close();
                }
            }
        });
    }
}

/// Without a GPU render node (VMs without a virtual GPU, containers) WebKit's web process
/// stalls initialising GL and the console page never loads. Mesa's software renderer
/// (llvmpipe) avoids that. A value the user set is kept.
fn use_software_gl_without_gpu() {
    let has_render_node = std::fs::read_dir("/dev/dri")
        .map(|dir| {
            dir.flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("renderD"))
        })
        .unwrap_or(false);
    if !has_render_node && std::env::var_os("LIBGL_ALWAYS_SOFTWARE").is_none() {
        // SAFETY: called first thing in main, before any other thread exists.
        unsafe { std::env::set_var("LIBGL_ALWAYS_SOFTWARE", "1") };
    }
}

/// The app icon, built in so windows and dialogs have it even when not installed.
fn set_default_icon() {
    const ICON: &[u8] = include_bytes!("../packaging/icons/rustdrac-256.png");
    let stream = gtk::gio::MemoryInputStream::from_bytes(&glib::Bytes::from_static(ICON));
    match gtk::gdk_pixbuf::Pixbuf::from_stream(&stream, None::<&gtk::gio::Cancellable>) {
        Ok(icon) => gtk::Window::set_default_icon(&icon),
        Err(e) => eprintln!("rustdrac: app icon: {e}"),
    }
}

/// Server details for [`App::export_logs`].
struct ExportServer {
    name: String,
    host: String,
    user: String,
    password: String,
    service_tag: String,
}

fn export_server(name: &str, host: &str, user: &str, password: &str, tag: &str) -> ExportServer {
    ExportServer {
        name: name.to_string(),
        host: host.to_string(),
        user: user.to_string(),
        password: password.to_string(),
        service_tag: tag.to_string(),
    }
}

fn header_button(label: &str, icon: &str, tip: &str) -> gtk::Button {
    let b = gtk::Button::with_label(label);
    b.set_image(Some(&gtk::Image::from_icon_name(
        Some(icon),
        gtk::IconSize::Button,
    )));
    b.set_always_show_image(true);
    b.set_tooltip_text(Some(tip));
    b.set_no_show_all(true);
    b
}

/// A short message at the bottom of the window, with "Open Folder" when it names a file.
struct Toast {
    revealer: gtk::Revealer,
    label: gtk::Label,
    open: gtk::Button,
    folder: Rc<RefCell<Option<PathBuf>>>,
    hide: Rc<RefCell<Option<glib::SourceId>>>,
}

impl Toast {
    fn new() -> Toast {
        let label = gtk::Label::new(None);
        label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        label.set_max_width_chars(70);
        let open = gtk::Button::with_label("Open Folder");
        let close = gtk::Button::from_icon_name(Some("window-close-symbolic"), gtk::IconSize::Menu);
        close.set_relief(gtk::ReliefStyle::None);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        row.style_context().add_class("app-toast");
        row.add(&label);
        row.add(&open);
        row.add(&close);
        let revealer = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideUp)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::End)
            .margin_bottom(18)
            .child(&row)
            .build();
        let folder: Rc<RefCell<Option<PathBuf>>> = Rc::default();
        let f = folder.clone();
        open.connect_clicked(move |_| {
            if let Some(dir) = f.borrow().as_ref() {
                let uri = gtk::gio::File::for_path(dir).uri();
                if let Err(e) = gtk::gio::AppInfo::launch_default_for_uri(
                    &uri,
                    None::<&gtk::gio::AppLaunchContext>,
                ) {
                    eprintln!("rustdrac: opening {}: {e}", dir.display());
                }
            }
        });
        let r = revealer.clone();
        close.connect_clicked(move |_| r.set_reveal_child(false));
        Toast {
            revealer,
            label,
            open,
            folder,
            hide: Rc::default(),
        }
    }

    fn show(&self, message: &str, folder: Option<PathBuf>) {
        self.label.set_text(message);
        self.open.set_visible(folder.is_some());
        *self.folder.borrow_mut() = folder;
        self.revealer.set_reveal_child(true);
        if let Some(id) = self.hide.borrow_mut().take() {
            id.remove();
        }
        let (r, hide) = (self.revealer.clone(), self.hide.clone());
        *self.hide.borrow_mut() = Some(glib::timeout_add_seconds_local_once(8, move || {
            hide.borrow_mut().take();
            r.set_reveal_child(false);
        }));
    }
}

/// `YYYYMMDD-HHMMSS` for file names.
fn now_stamp() -> String {
    glib::DateTime::now_local()
        .and_then(|d| d.format("%Y%m%d-%H%M%S"))
        .map(|s| s.to_string())
        .unwrap_or_default()
}

fn now_iso() -> String {
    glib::DateTime::now_local()
        .and_then(|d| d.format_iso8601())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

fn now_hms() -> String {
    glib::DateTime::now_local()
        .and_then(|d| d.format("%H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// Desktop notification over the freedesktop D-Bus API (works on KDE and GNOME).
fn notify(summary: &str, body: &str, health: Health) {
    let Ok(bus) =
        gtk::gio::bus_get_sync(gtk::gio::BusType::Session, None::<&gtk::gio::Cancellable>)
    else {
        return;
    };
    let urgency: u8 = if health == Health::Critical { 2 } else { 1 };
    let hints = glib::VariantDict::new(None);
    hints.insert_value("urgency", &urgency.to_variant());
    hints.insert_value("desktop-entry", &"rustdrac".to_variant());
    let icon = if health == Health::Critical {
        "dialog-error"
    } else {
        "dialog-warning"
    };
    // Notify(susssasa{sv}i); built by hand so `hints` is a dict, not a boxed variant.
    let args = glib::Variant::tuple_from_iter([
        "Rust Drac".to_variant(),
        0u32.to_variant(),
        icon.to_variant(),
        summary.to_variant(),
        body.to_variant(),
        Vec::<String>::new().to_variant(),
        hints.end(),
        (-1i32).to_variant(),
    ]);
    bus.call(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
        "Notify",
        Some(&args),
        None,
        gtk::gio::DBusCallFlags::NONE,
        5000,
        None::<&gtk::gio::Cancellable>,
        |res| {
            if let Err(e) = res {
                eprintln!("rustdrac: desktop notification: {e}");
            }
        },
    );
}

fn upgrade(weak: &Weak<AppInner>) -> Option<App> {
    weak.upgrade().map(App)
}

fn render_window(window: &gtk::Window) -> Result<gtk::cairo::Surface> {
    let alloc = window.allocation();
    let surface = gtk::cairo::ImageSurface::create(
        gtk::cairo::Format::ARgb32,
        alloc.width(),
        alloc.height(),
    )?;
    let cr = gtk::cairo::Context::new(&surface)?;
    // On X11, read back what is on screen: an offscreen draw misses the tree views'
    // column headers, which live in their own windows there.
    let on_screen = window
        .window()
        .filter(|_| std::env::var("GDK_BACKEND").as_deref() == Ok("x11"))
        .and_then(|w| w.pixbuf(0, 0, alloc.width(), alloc.height()));
    match on_screen {
        Some(pixbuf) => {
            cr.set_source_pixbuf(&pixbuf, 0.0, 0.0);
            cr.paint()?;
        }
        None => window.draw(&cr),
    }
    drop(cr);
    Ok((*surface).clone())
}

fn save_png(surface: gtk::cairo::Surface, path: &std::ffi::OsStr) -> Result<()> {
    let image = gtk::cairo::ImageSurface::try_from(surface)
        .map_err(|_| anyhow::anyhow!("snapshot is not an image surface"))?;
    image.write_to_png(&mut std::fs::File::create(path)?)?;
    Ok(())
}

fn report_snapshot(result: Result<()>) {
    if let Err(e) = result {
        eprintln!("rustdrac: snapshot: {e:#}");
    }
}

fn form_grid() -> gtk::Grid {
    gtk::Grid::builder()
        .row_spacing(8)
        .column_spacing(12)
        .margin(16)
        .build()
}

fn form_row(grid: &gtk::Grid, row: i32, label: &str, widget: &impl IsA<gtk::Widget>) {
    grid.attach(
        &gtk::Label::builder().label(label).xalign(1.0).build(),
        0,
        row,
        1,
        1,
    );
    widget.set_hexpand(true);
    grid.attach(widget, 1, row, 1, 1);
}

fn form_dialog(parent: &gtk::Window, title: &str) -> gtk::Dialog {
    let dialog = gtk::Dialog::with_buttons(
        Some(title),
        Some(parent),
        gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
        &[
            ("Cancel", gtk::ResponseType::Cancel),
            ("Connect", gtk::ResponseType::Ok),
        ],
    );
    dialog.set_default_response(gtk::ResponseType::Ok);
    dialog.set_default_size(420, -1);
    dialog
}

fn password_entry() -> gtk::Entry {
    gtk::Entry::builder()
        .visibility(false)
        .input_purpose(gtk::InputPurpose::Password)
        .activates_default(true)
        .build()
}

/// Ask for the password of a saved connection. Returns (password, remember).
fn password_dialog(parent: &gtk::Window, conn: &Connection, user: &str) -> Option<(String, bool)> {
    let dialog = form_dialog(parent, &format!("Connect to {}", conn.display_name()));
    let grid = form_grid();
    let info = gtk::Label::new(Some(&format!("{user}@{}", conn.address)));
    info.set_xalign(0.0);
    let password = password_entry();
    let remember = gtk::CheckButton::with_label("Save password in the system keyring");
    form_row(&grid, 0, "Server", &info);
    form_row(&grid, 1, "Password", &password);
    form_row(&grid, 2, "", &remember);
    dialog.content_area().add(&grid);
    dialog.show_all();
    password.grab_focus();
    let ok = dialog.run() == gtk::ResponseType::Ok;
    let result = ok.then(|| (password.text().to_string(), remember.is_active()));
    // SAFETY: the dialog is not used after this point.
    unsafe { dialog.destroy() };
    result
}

struct QuickConnect {
    host: String,
    user: String,
    password: String,
    add_to_book: bool,
    save_password: bool,
}

fn quick_connect_dialog(parent: &gtk::Window, host: &str, user: &str) -> Option<QuickConnect> {
    let dialog = form_dialog(parent, "Quick Connect");
    let grid = form_grid();
    let host_entry = gtk::Entry::builder()
        .text(host)
        .placeholder_text("hostname or IP of the iDRAC")
        .activates_default(true)
        .build();
    let user_entry = gtk::Entry::builder()
        .text(user)
        .activates_default(true)
        .build();
    let password = password_entry();
    let add = gtk::CheckButton::with_label("Add to the address book");
    let save = gtk::CheckButton::with_label("Save password in the system keyring");
    save.set_sensitive(false);
    let s = save.clone();
    add.connect_toggled(move |b| {
        s.set_sensitive(b.is_active());
        if !b.is_active() {
            s.set_active(false);
        }
    });
    form_row(&grid, 0, "Host", &host_entry);
    form_row(&grid, 1, "User", &user_entry);
    form_row(&grid, 2, "Password", &password);
    form_row(&grid, 3, "", &add);
    form_row(&grid, 4, "", &save);
    dialog.content_area().add(&grid);
    dialog.show_all();
    if !host.is_empty() {
        password.grab_focus();
    }

    let result = loop {
        if dialog.run() != gtk::ResponseType::Ok {
            break None;
        }
        let host = host_entry.text().trim().to_string();
        if host.is_empty() {
            host_entry.grab_focus();
            continue;
        }
        break Some(QuickConnect {
            host,
            user: user_entry.text().trim().to_string(),
            password: password.text().to_string(),
            add_to_book: add.is_active(),
            save_password: save.is_active(),
        });
    };
    // SAFETY: the dialog is not used after this point.
    unsafe { dialog.destroy() };
    result
}
