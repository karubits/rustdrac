//! The "Health & SEL" tab for one server: component health and the System Event Log.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use anyhow::Result;
use gtk::glib;
use gtk::prelude::*;
use reqwest::blocking::Client;

use crate::bmc::Vendor;
use crate::redfish::{self, Health, HealthReport, Redfish, SelEntry, Step};
use crate::theme::health_markup;

/// Who to ask.
#[derive(Debug, Clone)]
pub struct Target {
    pub name: String,
    pub host: String,
    pub user: String,
    pub password: String,
    pub connection_id: Option<String>,
    /// The saved vendor, if known; otherwise it is detected.
    pub vendor: Option<Vendor>,
}

type ReportHook = Box<dyn Fn(&Target, &Result<HealthReport, String>)>;
type PageHook = Box<dyn Fn(&gtk::Widget)>;

pub struct Hooks {
    /// A fresh health report (or the error), so the address book's alarm column can update.
    pub on_report: ReportHook,
    pub on_closed: PageHook,
}

// Component list columns.
const C_STATUS: u32 = 0;
const C_CATEGORY: u32 = 1;
const C_NAME: u32 = 2;
const C_READING: u32 = 3;
// SEL columns.
const S_CREATED: u32 = 0;
const S_SEVERITY: u32 = 1;
const S_EVENT: u32 = 2;
const S_SENSOR: u32 = 3;
const S_MESSAGE: u32 = 4;

struct Inner {
    target: Target,
    client: Client,
    hooks: Hooks,
    root: gtk::Box,
    tab: gtk::Box,
    tab_spinner: gtk::Spinner,
    overall: gtk::Label,
    subtitle: gtk::Label,
    spinner: gtk::Spinner,
    buttons: Vec<gtk::Button>,
    only_problems: gtk::CheckButton,
    components: gtk::ListStore,
    severity: gtk::ComboBoxText,
    search: gtk::SearchEntry,
    sel_count: gtk::Label,
    sel_model: gtk::ListStore,
    sel_view: gtk::TreeView,
    /// Entries behind the rows of `sel_model`, in row order.
    shown: RefCell<Vec<SelEntry>>,
    /// Restores the entry count after a "copied" flash.
    count_flash: RefCell<Option<glib::SourceId>>,
    report: RefCell<Option<HealthReport>>,
    sel: RefCell<Vec<SelEntry>>,
    pending: std::cell::Cell<u8>,
}

#[derive(Clone)]
pub struct HealthTab(Rc<Inner>);

impl HealthTab {
    pub fn open(target: Target, client: Client, hooks: Hooks) -> HealthTab {
        // Header: overall state, power, last refresh, actions.
        let overall = gtk::Label::builder().xalign(0.0).use_markup(true).build();
        overall.style_context().add_class("health-overall");
        let subtitle = gtk::Label::builder().xalign(0.0).build();
        subtitle.style_context().add_class("dim-label");
        let titles = gtk::Box::new(gtk::Orientation::Vertical, 2);
        titles.add(&overall);
        titles.add(&subtitle);
        let spinner = gtk::Spinner::new();
        let refresh = icon_button(
            "Refresh",
            "view-refresh-symbolic",
            "Read health and the SEL again",
        );
        refresh.style_context().add_class("suggested-action");
        let export = icon_button(
            "Export SEL…",
            "document-save-symbolic",
            "Save the SEL as CSV",
        );
        let clear = icon_button(
            "Clear SEL…",
            "edit-clear-all-symbolic",
            "Delete every SEL entry on the BMC",
        );
        clear.style_context().add_class("destructive-action");
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        header.pack_start(&titles, true, true, 0);
        header.pack_start(&spinner, false, false, 0);
        for b in [&refresh, &export, &clear] {
            header.pack_start(b, false, false, 0);
        }

        // Components.
        let components = gtk::ListStore::new(&[String::static_type(); 4]);
        let comp_view = list_view(
            &components,
            &[
                ("Status", C_STATUS, 110, true),
                ("Component", C_CATEGORY, 140, false),
                ("Name", C_NAME, 280, false),
                ("Reading", C_READING, 0, false),
            ],
        );
        let only_problems = gtk::CheckButton::with_label("Only show problems");
        let comp_bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        comp_bar.pack_start(&section_label("Components"), false, false, 0);
        comp_bar.pack_end(&only_problems, false, false, 0);
        let comp_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        comp_box.add(&comp_bar);
        comp_box.pack_start(&scrolled(&comp_view), true, true, 0);

        // SEL.
        let sel_model = gtk::ListStore::new(&[String::static_type(); 5]);
        let sel_view = list_view(
            &sel_model,
            &[
                ("Date", S_CREATED, 190, false),
                ("Severity", S_SEVERITY, 110, true),
                ("Event", S_EVENT, 90, false),
                ("Sensor", S_SENSOR, 220, false),
                ("Message", S_MESSAGE, 0, false),
            ],
        );
        sel_view.set_tooltip_column(S_MESSAGE as i32);
        sel_view.selection().set_mode(gtk::SelectionMode::Multiple);
        let severity = gtk::ComboBoxText::new();
        severity.append(Some("all"), "All severities");
        severity.append(Some("warning"), "Warning and critical");
        severity.append(Some("critical"), "Critical only");
        severity.set_active_id(Some("all"));
        let search = gtk::SearchEntry::builder()
            .placeholder_text("Search the SEL…")
            .width_chars(28)
            .build();
        search.style_context().add_class("search-pill");
        let sel_count = gtk::Label::new(None);
        sel_count.style_context().add_class("dim-label");
        let sel_bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        sel_bar.pack_start(&section_label("System Event Log"), false, false, 0);
        sel_bar.pack_start(&sel_count, false, false, 0);
        sel_bar.pack_end(&search, false, false, 0);
        sel_bar.pack_end(&severity, false, false, 0);
        let sel_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        sel_box.add(&sel_bar);
        sel_box.pack_start(&scrolled(&sel_view), true, true, 0);

        let paned = gtk::Paned::new(gtk::Orientation::Vertical);
        paned.pack1(&comp_box, true, false);
        paned.pack2(&sel_box, true, false);
        paned.set_position(330);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
        root.set_border_width(14);
        root.add(&header);
        root.pack_start(&paned, true, true, 0);
        root.show_all();
        spinner.hide();

        // Tab label.
        let tab_spinner = gtk::Spinner::new();
        let tab_close =
            gtk::Button::from_icon_name(Some("window-close-symbolic"), gtk::IconSize::Menu);
        tab_close.set_relief(gtk::ReliefStyle::None);
        tab_close.set_tooltip_text(Some("Close"));
        let tab = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        tab.add(&gtk::Image::from_icon_name(
            Some("emblem-important-symbolic"),
            gtk::IconSize::Menu,
        ));
        tab.add(&tab_spinner);
        tab.add(&gtk::Label::new(Some(&format!("{} · Health", target.name))));
        tab.add(&tab_close);
        tab.show_all();

        let this = HealthTab(Rc::new(Inner {
            target,
            client,
            hooks,
            root,
            tab,
            tab_spinner,
            overall,
            subtitle,
            spinner,
            buttons: vec![refresh.clone(), export.clone(), clear.clone()],
            only_problems,
            components,
            severity,
            search,
            sel_count,
            sel_model,
            sel_view,
            shown: RefCell::new(Vec::new()),
            count_flash: RefCell::new(None),
            report: RefCell::new(None),
            sel: RefCell::new(Vec::new()),
            pending: std::cell::Cell::new(0),
        }));

        let t = this.downgrade();
        refresh.connect_clicked(move |_| with(&t, |t| t.refresh()));
        let t = this.downgrade();
        export.connect_clicked(move |_| with(&t, |t| t.export()));
        let t = this.downgrade();
        clear.connect_clicked(move |_| with(&t, |t| t.clear_sel()));
        let t = this.downgrade();
        tab_close.connect_clicked(move |_| with(&t, |t| (t.0.hooks.on_closed)(&t.widget())));
        let t = this.downgrade();
        this.0
            .only_problems
            .connect_toggled(move |_| with(&t, |t| t.show_components()));
        let t = this.downgrade();
        this.0
            .severity
            .connect_changed(move |_| with(&t, |t| t.show_sel()));
        let t = this.downgrade();
        this.0
            .search
            .connect_search_changed(move |_| with(&t, |t| t.show_sel()));

        // Right click: copy menu (the row under the pointer joins the selection first).
        let t = this.downgrade();
        this.0.sel_view.connect_button_press_event(move |view, ev| {
            if ev.button() != 3 || ev.event_type() != gtk::gdk::EventType::ButtonPress {
                return glib::Propagation::Proceed;
            }
            let (x, y) = ev.position();
            if let Some((Some(path), ..)) = view.path_at_pos(x as i32, y as i32)
                && !view.selection().path_is_selected(&path)
            {
                view.selection().unselect_all();
                view.selection().select_path(&path);
            }
            with(&t, |t| t.sel_menu(ev));
            glib::Propagation::Stop
        });
        // Ctrl+C copies the selected lines.
        let t = this.downgrade();
        this.0.sel_view.connect_key_press_event(move |_, ev| {
            let ctrl = ev.state().contains(gtk::gdk::ModifierType::CONTROL_MASK);
            if ctrl && matches!(ev.keyval().to_unicode(), Some('c' | 'C')) {
                with(&t, |t| t.copy_selected_lines());
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });

        this.refresh();
        this
    }

    pub fn widget(&self) -> gtk::Widget {
        self.0.root.clone().upcast()
    }

    pub fn tab_widget(&self) -> gtk::Widget {
        self.0.tab.clone().upcast()
    }

    pub fn title(&self) -> String {
        format!("{} · Health & SEL", self.0.target.name)
    }

    pub fn target(&self) -> &Target {
        &self.0.target
    }

    pub fn connection_id(&self) -> Option<&str> {
        self.0.target.connection_id.as_deref()
    }

    fn downgrade(&self) -> Weak<Inner> {
        Rc::downgrade(&self.0)
    }

    fn parent_window(&self) -> Option<gtk::Window> {
        self.0.root.toplevel().and_then(|w| w.downcast().ok())
    }

    fn busy(&self, delta: i8) {
        let n = (self.0.pending.get() as i8 + delta).max(0) as u8;
        self.0.pending.set(n);
        let busy = n > 0;
        for s in [&self.0.spinner, &self.0.tab_spinner] {
            s.set_visible(busy);
            if busy {
                s.start();
            } else {
                s.stop();
            }
        }
        for b in &self.0.buttons {
            b.set_sensitive(!busy);
        }
    }

    pub fn refresh(&self) {
        if self.0.pending.get() > 0 {
            return;
        }
        if self.0.report.borrow().is_none() {
            self.0.overall.set_markup("<b>Reading health…</b>");
            self.0
                .subtitle
                .set_text(&format!("{} as {}", self.0.target.host, self.0.target.user));
        }
        self.busy(2);

        let t = &self.0.target;
        let rf = Redfish::new(&self.0.client, &t.host, &t.user, &t.password).with_vendor(t.vendor);

        // The SEL in parallel (its own connection), health step by step on another, showing
        // each part as it arrives: an iDRAC 8 takes several seconds per Redfish request.
        let sel_rf = rf.clone();
        let weak = self.downgrade();
        glib::MainContext::default().spawn_local(async move {
            let result = blocking(move || sel_rf.sel()).await;
            with(&weak, |t| t.got_sel(result));
        });

        let weak = self.downgrade();
        glib::MainContext::default().spawn_local(async move {
            let r = rf.clone();
            let mut report = match blocking(move || r.system()).await {
                Ok(report) => report,
                Err(e) => return with(&weak, |t| t.got_health(Err(e), None)),
            };
            for step in Step::ALL {
                let progress = format!("reading {}…", step.label());
                with(&weak, |t| t.got_health(Ok(report.clone()), Some(&progress)));
                if weak.upgrade().is_none() {
                    return;
                }
                let r = rf.clone();
                let drives = match blocking(move || r.step(step)).await {
                    Ok((sensors, drives)) => {
                        report.add_sensors(sensors);
                        drives
                    }
                    Err(e) => {
                        report.add(step, Err(e));
                        Vec::new()
                    }
                };
                let count = drives.len();
                for (i, drive) in drives.into_iter().enumerate() {
                    let progress = format!("reading drive {} of {count}…", i + 1);
                    with(&weak, |t| t.got_health(Ok(report.clone()), Some(&progress)));
                    let r = rf.clone();
                    match blocking(move || r.drive(&drive)).await {
                        Ok(Some(sensor)) => report.add_sensors(vec![sensor]),
                        Ok(None) => {}
                        Err(e) => report.add(step, Err(e)),
                    }
                }
            }
            with(&weak, |t| t.got_health(Ok(report), None));
        });
    }

    /// Show a (partial) report. `progress` is set while more is being read; when it is
    /// `None` the report is final and is passed on to the address book.
    fn got_health(&self, result: Result<HealthReport>, progress: Option<&str>) {
        let first = self.0.report.borrow().is_none();
        if progress.is_none() {
            self.busy(-1);
            let shared = result
                .as_ref()
                .map(Clone::clone)
                .map_err(|e| format!("{e:#}"));
            (self.0.hooks.on_report)(&self.0.target, &shared);
        }
        let now = glib::DateTime::now_local()
            .and_then(|d| d.format("%H:%M:%S"))
            .map(|s| s.to_string())
            .unwrap_or_default();
        match result {
            Ok(report) => {
                let alarms = report.alarms().len();
                self.0.overall.set_markup(&format!(
                    "<span size='x-large'>{}</span>   <span size='large'>{}</span>",
                    health_markup(report.overall),
                    glib::markup_escape_text(&match alarms {
                        0 => "no active alarms".to_string(),
                        1 => "1 component needs attention".to_string(),
                        n => format!("{n} components need attention"),
                    })
                ));
                self.0.subtitle.set_text(&format!(
                    "{} · power {} · {} components · {}",
                    self.0.target.host,
                    report.power_state,
                    report.sensors.len(),
                    progress.map_or_else(|| format!("checked {now}"), str::to_string),
                ));
                if first || progress.is_none() {
                    self.0.only_problems.set_active(alarms > 0);
                }
                *self.0.report.borrow_mut() = Some(report);
                self.show_components();
            }
            Err(e) => {
                self.0.overall.set_markup(&format!(
                    "<span size='x-large'>{}</span>",
                    health_markup(Health::Unknown)
                ));
                self.0.subtitle.set_text(&format!("{e:#}"));
            }
        }
    }

    fn got_sel(&self, result: Result<Vec<SelEntry>>) {
        self.busy(-1);
        match result {
            Ok(entries) => {
                *self.0.sel.borrow_mut() = entries;
                self.show_sel();
                self.show_components();
            }
            Err(e) => {
                self.0.sel_model.clear();
                self.0
                    .sel_count
                    .set_text(&format!("could not read the SEL: {e:#}"));
            }
        }
    }

    fn show_components(&self) {
        let model = &self.0.components;
        model.clear();
        let report = self.0.report.borrow();
        let Some(report) = report.as_ref() else {
            return;
        };
        let only_problems = self.0.only_problems.is_active();
        let mut sensors: Vec<_> = report
            .sensors
            .iter()
            .filter(|s| !only_problems || s.health != Health::Ok)
            .collect();
        // When the rollup is in alarm, the SEL usually says why (e.g. a CPU machine check
        // while every component still reads OK).
        let sel_alarms = if report.overall.is_alarm() {
            redfish::sel_alarms(&self.0.sel.borrow())
        } else {
            Vec::new()
        };
        sensors.extend(sel_alarms.iter());
        // Problems first, then in reading order.
        sensors.sort_by_key(|s| std::cmp::Reverse(s.health));
        for s in sensors {
            model.insert_with_values(
                None,
                &[
                    (C_STATUS, &health_markup(s.health)),
                    (C_CATEGORY, &s.category),
                    (C_NAME, &s.name),
                    (C_READING, &s.reading),
                ],
            );
        }
    }

    fn sel_filtered(&self) -> Vec<SelEntry> {
        let min = match self.0.severity.active_id().as_deref() {
            Some("critical") => Health::Critical,
            Some("warning") => Health::Warning,
            _ => Health::Ok,
        };
        let query = self.0.search.text().to_lowercase();
        self.0
            .sel
            .borrow()
            .iter()
            .filter(|e| min == Health::Ok || e.severity >= min)
            .filter(|e| {
                query.split_whitespace().all(|term| {
                    [
                        &e.message,
                        &e.sensor,
                        &e.created,
                        &e.event,
                        e.severity.label(),
                    ]
                    .iter()
                    .any(|f| f.to_lowercase().contains(term))
                })
            })
            .cloned()
            .collect()
    }

    fn show_sel(&self) {
        let model = &self.0.sel_model;
        model.clear();
        let entries = self.sel_filtered();
        for e in &entries {
            model.insert_with_values(
                None,
                &[
                    (S_CREATED, &pretty_time(&e.created)),
                    (S_SEVERITY, &health_markup(e.severity)),
                    (S_EVENT, &e.event),
                    (S_SENSOR, &e.sensor),
                    (S_MESSAGE, &e.message),
                ],
            );
        }
        *self.0.shown.borrow_mut() = entries;
        self.show_sel_count();
    }

    fn show_sel_count(&self) {
        let shown = self.0.shown.borrow().len();
        let total = self.0.sel.borrow().len();
        let alarms = self
            .0
            .sel
            .borrow()
            .iter()
            .filter(|e| e.severity.is_alarm())
            .count();
        self.0.sel_count.set_text(&if shown == total {
            format!("{total} entries, {alarms} warning/critical")
        } else {
            format!("{shown} of {total} entries")
        });
    }

    /// The entries of the selected rows, in display order.
    fn selected_entries(&self) -> Vec<SelEntry> {
        let (rows, _) = self.0.sel_view.selection().selected_rows();
        let shown = self.0.shown.borrow();
        rows.iter()
            .filter_map(|p| p.indices().first().copied())
            .filter_map(|i| shown.get(i as usize).cloned())
            .collect()
    }

    fn sel_menu(&self, ev: &gtk::gdk::EventButton) {
        let selected = self.selected_entries().len();
        let shown = self.0.shown.borrow().len();
        let menu = gtk::Menu::new();
        let item = |label: String, enabled: bool, f: Box<dyn Fn(&HealthTab)>| {
            let mi = gtk::MenuItem::with_label(&label);
            mi.set_sensitive(enabled);
            let t = self.clone();
            mi.connect_activate(move |_| f(&t));
            menu.append(&mi);
        };
        let lines = if selected > 1 {
            format!("Copy {selected} Lines")
        } else {
            "Copy Line".to_string()
        };
        item(lines, selected > 0, Box::new(|t| t.copy_selected_lines()));
        item(
            "Copy Message".to_string(),
            selected > 0,
            Box::new(|t| {
                let text: Vec<String> = t
                    .selected_entries()
                    .into_iter()
                    .map(|e| e.message)
                    .collect();
                t.copy(&text.join("\n"), text.len(), "message");
            }),
        );
        item(
            format!("Copy All Shown ({shown})"),
            shown > 0,
            Box::new(|t| {
                let shown = t.0.shown.borrow();
                let text = std::iter::once(SEL_HEADER.to_string())
                    .chain(shown.iter().map(sel_line))
                    .collect::<Vec<_>>()
                    .join("\n");
                t.copy(&text, shown.len(), "line");
            }),
        );
        menu.show_all();
        menu.popup_at_pointer(Some(ev));
    }

    fn copy_selected_lines(&self) {
        let lines: Vec<String> = self.selected_entries().iter().map(sel_line).collect();
        if !lines.is_empty() {
            self.copy(&lines.join("\n"), lines.len(), "line");
        }
    }

    /// Puts `text` on the clipboard and says so in place of the entry count for a moment.
    fn copy(&self, text: &str, count: usize, what: &str) {
        gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD).set_text(text);
        let plural = if count == 1 { "" } else { "s" };
        self.0
            .sel_count
            .set_text(&format!("copied {count} {what}{plural}"));
        if let Some(id) = self.0.count_flash.borrow_mut().take() {
            id.remove();
        }
        let weak = self.downgrade();
        let id = glib::timeout_add_local_once(std::time::Duration::from_millis(1800), move || {
            with(&weak, |t| {
                t.0.count_flash.borrow_mut().take();
                t.show_sel_count();
            });
        });
        *self.0.count_flash.borrow_mut() = Some(id);
    }

    fn export(&self) {
        let dialog = gtk::FileChooserNative::new(
            Some("Export SEL"),
            self.parent_window().as_ref(),
            gtk::FileChooserAction::Save,
            Some("Save"),
            Some("Cancel"),
        );
        dialog.set_do_overwrite_confirmation(true);
        dialog.set_current_name(&format!(
            "sel-{}.csv",
            self.0
                .target
                .name
                .replace(|c: char| !c.is_alphanumeric() && c != '-', "_")
        ));
        if dialog.run() != gtk::ResponseType::Accept {
            return;
        }
        let Some(path) = dialog.filename() else {
            return;
        };
        let csv = redfish::sel_csv(&self.sel_filtered());
        if let Err(e) = std::fs::write(&path, csv) {
            crate::manager::error_dialog(
                self.parent_window().as_ref(),
                &format!("{}: {e}", path.display()),
            );
        }
    }

    fn clear_sel(&self) {
        let dialog = gtk::MessageDialog::new(
            self.parent_window().as_ref(),
            gtk::DialogFlags::MODAL,
            gtk::MessageType::Warning,
            gtk::ButtonsType::None,
            &format!("Clear the System Event Log on {}?", self.0.target.name),
        );
        dialog.set_secondary_text(Some(
            "Every SEL entry on the BMC is deleted. This cannot be undone — export it first if you need a record.",
        ));
        dialog.add_buttons(&[
            ("Cancel", gtk::ResponseType::Cancel),
            ("Clear SEL", gtk::ResponseType::Accept),
        ]);
        let response = dialog.run();
        // SAFETY: the dialog is not used after this point.
        unsafe { dialog.destroy() };
        if response != gtk::ResponseType::Accept {
            return;
        }
        self.busy(1);
        let target = self.0.target.clone();
        let client = self.0.client.clone();
        let job = gtk::gio::spawn_blocking(move || {
            Redfish::new(&client, &target.host, &target.user, &target.password)
                .with_vendor(target.vendor)
                .clear_sel()
        });
        let t = self.downgrade();
        glib::MainContext::default().spawn_local(async move {
            let result = job
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("task panicked")));
            with(&t, |t| {
                t.busy(-1);
                if let Err(e) = result {
                    crate::manager::error_dialog(t.parent_window().as_ref(), &format!("{e:#}"));
                }
                t.refresh();
            });
        });
    }
}

/// Run blocking Redfish work off the UI thread.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    gtk::gio::spawn_blocking(f)
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("background task panicked")))
}

fn with(weak: &Weak<Inner>, f: impl FnOnce(&HealthTab)) {
    if let Some(inner) = weak.upgrade() {
        f(&HealthTab(inner));
    }
}

/// Header for "Copy All Shown"; the columns of [`sel_line`].
const SEL_HEADER: &str = "Date\tSeverity\tEvent\tSensor\tMessage";

/// One SEL entry as a tab-separated line, as shown in the list.
fn sel_line(e: &SelEntry) -> String {
    format!(
        "{}\t{}\t{}\t{}\t{}",
        pretty_time(&e.created),
        e.severity.label(),
        e.event,
        e.sensor,
        e.message
    )
}

/// "2026-10-30T02:03:59+09:00" -> "2026-10-30 02:03:59 +09:00"
fn pretty_time(s: &str) -> String {
    match s.split_once('T') {
        Some((date, rest)) => {
            let split = rest.find(['+', '-', 'Z']).unwrap_or(rest.len());
            let (time, zone) = rest.split_at(split);
            // Supermicro stamps entries in UTC with a bare "Z".
            let zone = if zone == "Z" { "UTC" } else { zone };
            format!("{date} {time} {zone}").trim_end().to_string()
        }
        None => s.to_string(),
    }
}

fn icon_button(label: &str, icon: &str, tip: &str) -> gtk::Button {
    let b = gtk::Button::with_label(label);
    b.set_image(Some(&gtk::Image::from_icon_name(
        Some(icon),
        gtk::IconSize::Button,
    )));
    b.set_always_show_image(true);
    b.set_tooltip_text(Some(tip));
    b
}

fn section_label(text: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.style_context().add_class("section-title");
    l
}

fn scrolled(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    let s = gtk::ScrolledWindow::builder()
        .shadow_type(gtk::ShadowType::In)
        .vexpand(true)
        .build();
    s.add(child);
    s
}

/// A list view; columns are (title, model column, width (0 = expand), markup?).
fn list_view(model: &gtk::ListStore, columns: &[(&str, u32, i32, bool)]) -> gtk::TreeView {
    let view = gtk::TreeView::with_model(model);
    view.set_enable_search(false);
    for &(title, col, width, markup) in columns {
        let cell = gtk::CellRendererText::new();
        cell.set_padding(6, 4);
        cell.set_property("ellipsize", gtk::pango::EllipsizeMode::End);
        let column = gtk::TreeViewColumn::new();
        column.set_title(title);
        column.set_resizable(true);
        if width > 0 {
            column.set_sizing(gtk::TreeViewColumnSizing::Fixed);
            column.set_fixed_width(width);
        } else {
            column.set_expand(true);
        }
        CellLayoutExt::pack_start(&column, &cell, true);
        CellLayoutExt::add_attribute(
            &column,
            &cell,
            if markup { "markup" } else { "text" },
            col as i32,
        );
        view.append_column(&column);
    }
    view
}

#[cfg(test)]
mod tests {
    use crate::redfish::{Health, SelEntry};

    #[test]
    fn copied_lines_are_tab_separated() {
        let e = SelEntry {
            id: 7,
            created: "2026-11-01T19:57:46-06:00".into(),
            severity: Health::Critical,
            message: "CPU 1 machine check error detected.".into(),
            sensor: "Processor".into(),
            event: "Transition to Non-recoverable".into(),
        };
        assert_eq!(
            super::sel_line(&e),
            "2026-11-01 19:57:46 -06:00\tCritical\tTransition to Non-recoverable\tProcessor\tCPU 1 machine check error detected."
        );
        assert_eq!(
            super::SEL_HEADER.split('\t').count(),
            super::sel_line(&e).split('\t').count()
        );
    }

    #[test]
    fn pretty_time() {
        assert_eq!(
            super::pretty_time("2026-10-30T02:03:59+09:00"),
            "2026-10-30 02:03:59 +09:00"
        );
        assert_eq!(
            super::pretty_time("2026-10-30T02:03:59Z"),
            "2026-10-30 02:03:59 UTC"
        );
        assert_eq!(
            super::pretty_time("2026-10-30T02:03:59"),
            "2026-10-30 02:03:59"
        );
    }
}
