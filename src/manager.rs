//! The "Connections" tab: a searchable, grouped address book of BMCs.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

use crate::bmc::Vendor;
use crate::redfish::{Health, HealthReport};
use crate::store::{self, Connection, Store};
use crate::theme::{dot_markup, health_color, health_markup};

const COL_NAME: u32 = 0;
const COL_ADDRESS: u32 = 1;
const COL_USER: u32 = 2;
const COL_TAGS: u32 = 3;
const COL_SERVICE_TAG: u32 = 4;
const COL_MODEL: u32 = 5;
const COL_NOTES: u32 = 6;
const COL_ID: u32 = 7;
const COL_WEIGHT: u32 = 8;
const COL_TOOLTIP: u32 = 9;
const COL_HEALTH: u32 = 10;

/// Latest alarm-check state of a saved connection.
#[derive(Debug, Clone)]
pub enum Check {
    Checking,
    NoPassword,
    Failed(String),
    Done(HealthReport),
}

impl Check {
    pub fn health(&self) -> Option<Health> {
        match self {
            Check::Done(r) => Some(r.overall),
            _ => None,
        }
    }

    fn markup(&self) -> String {
        match self {
            Check::Checking => dot_markup("#8b949e", "checking…"),
            Check::NoPassword => "<span alpha='55%'>no password</span>".to_string(),
            Check::Failed(_) => dot_markup(health_color(Health::Unknown), "Unreachable"),
            Check::Done(r) => health_markup(r.overall),
        }
    }

    fn tooltip(&self) -> String {
        match self {
            Check::Checking => "Checking health…".into(),
            Check::NoPassword => {
                "Save the password (Edit) to include this server in alarm checks.".into()
            }
            Check::Failed(e) => format!("Health check failed: {e}"),
            Check::Done(r) => r.summary(),
        }
    }
}

pub struct Hooks {
    /// Open a console for this saved connection.
    pub connect: Box<dyn Fn(&Connection)>,
    /// Open the ad-hoc "Quick Connect" dialog.
    pub quick_connect: Box<dyn Fn()>,
    /// Open the Health & SEL tab for this saved connection.
    pub health: Box<dyn Fn(&Connection)>,
    /// Run the alarm check on every server with a saved password.
    pub check_all: Box<dyn Fn()>,
    /// Export logs and a hardware summary for this saved connection.
    pub export: Box<dyn Fn(&Connection)>,
}

struct Inner {
    store: Rc<RefCell<Store>>,
    hooks: Hooks,
    root: gtk::Box,
    search: gtk::SearchEntry,
    tree: gtk::TreeView,
    model: gtk::TreeStore,
    footer: gtk::Label,
    list: gtk::Stack,
    empty_title: gtk::Label,
    empty_hint: gtk::Label,
    edit_buttons: Vec<gtk::Button>,
    checks: RefCell<HashMap<String, Check>>,
    last_check: RefCell<Option<String>>,
}

#[derive(Clone)]
pub struct Manager(Rc<Inner>);

impl Manager {
    pub fn new(store: Rc<RefCell<Store>>, hooks: Hooks) -> Manager {
        let search = gtk::SearchEntry::builder()
            .placeholder_text(
                "Search name, address, group, tag, service tag / serial, model, notes…",
            )
            .hexpand(true)
            .build();
        let button = |label: &str, icon: &str, tip: &str| {
            let b = gtk::Button::with_label(label);
            b.set_image(Some(&gtk::Image::from_icon_name(
                Some(icon),
                gtk::IconSize::Button,
            )));
            b.set_always_show_image(true);
            b.set_tooltip_text(Some(tip));
            b
        };
        let connect = button(
            "Connect",
            "media-playback-start-symbolic",
            "Open the console (Enter / double-click)",
        );
        let quick = button(
            "Quick Connect",
            "network-server-symbolic",
            "Connect to a server that is not in the address book",
        );
        let add = button(
            "Add",
            "list-add-symbolic",
            "Add a server to the address book",
        );
        let health = button(
            "Health & SEL",
            "emblem-important-symbolic",
            "Component health and the System Event Log of the selected server",
        );
        let check = button(
            "Check All",
            "emblem-synchronizing-symbolic",
            "Check every server with a saved password for alarms",
        );
        let edit = button("Edit", "document-edit-symbolic", "Edit the selected server");
        let delete = button(
            "Delete",
            "user-trash-symbolic",
            "Remove the selected server",
        );

        search.style_context().add_class("search-pill");
        connect.style_context().add_class("suggested-action");
        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        toolbar.add(&search);
        for b in [&connect, &health, &check, &quick, &add, &edit, &delete] {
            toolbar.add(b);
        }

        let model = gtk::TreeStore::new(&[
            String::static_type(),
            String::static_type(),
            String::static_type(),
            String::static_type(),
            String::static_type(),
            String::static_type(),
            String::static_type(),
            String::static_type(),
            i32::static_type(),
            String::static_type(),
            String::static_type(),
        ]);
        let tree = gtk::TreeView::with_model(&model);
        tree.set_tooltip_column(COL_TOOLTIP as i32);
        tree.set_enable_search(false);
        {
            let cell = gtk::CellRendererText::new();
            cell.set_padding(6, 5);
            let column = gtk::TreeViewColumn::new();
            column.set_title("Health");
            column.set_resizable(true);
            column.set_sizing(gtk::TreeViewColumnSizing::Fixed);
            column.set_fixed_width(150);
            CellLayoutExt::pack_start(&column, &cell, true);
            CellLayoutExt::add_attribute(&column, &cell, "markup", COL_HEALTH as i32);
            tree.append_column(&column);
        }
        for (title, col, width) in [
            ("Name", COL_NAME, 220),
            ("Address", COL_ADDRESS, 190),
            ("User", COL_USER, 110),
            ("Tags", COL_TAGS, 170),
            ("Service Tag / Serial", COL_SERVICE_TAG, 130),
            ("Model", COL_MODEL, 170),
            ("Notes", COL_NOTES, 0),
        ] {
            let cell = gtk::CellRendererText::new();
            cell.set_padding(6, 5);
            cell.set_property("ellipsize", gtk::pango::EllipsizeMode::End);
            let column = gtk::TreeViewColumn::new();
            column.set_title(title);
            column.set_resizable(true);
            // Fixed starting widths; Notes takes the rest.
            if width > 0 {
                column.set_sizing(gtk::TreeViewColumnSizing::Fixed);
                column.set_fixed_width(width);
            } else {
                column.set_expand(true);
            }
            CellLayoutExt::pack_start(&column, &cell, true);
            CellLayoutExt::add_attribute(&column, &cell, "text", col as i32);
            CellLayoutExt::add_attribute(&column, &cell, "weight", COL_WEIGHT as i32);
            tree.append_column(&column);
        }
        let scrolled = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .shadow_type(gtk::ShadowType::In)
            .build();
        scrolled.add(&tree);

        // Shown instead of the list when the address book is empty or nothing matches.
        let empty_title = gtk::Label::new(None);
        empty_title.style_context().add_class("empty-title");
        let empty_hint = gtk::Label::new(None);
        empty_hint.style_context().add_class("empty-hint");
        let empty_icon =
            gtk::Image::from_icon_name(Some("network-server-symbolic"), gtk::IconSize::Dialog);
        empty_icon.set_pixel_size(96);
        empty_icon.style_context().add_class("empty-icon");
        let empty = gtk::Box::new(gtk::Orientation::Vertical, 12);
        empty.set_valign(gtk::Align::Center);
        empty.add(&empty_icon);
        empty.add(&empty_title);
        empty.add(&empty_hint);
        let list = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .vexpand(true)
            .build();
        list.add_named(&scrolled, "list");
        list.add_named(&empty, "empty");

        let footer = gtk::Label::builder().xalign(0.0).selectable(true).build();
        footer.style_context().add_class("manager-footer");

        let root = gtk::Box::new(gtk::Orientation::Vertical, 10);
        root.set_border_width(14);
        root.add(&toolbar);
        root.add(&list);
        root.add(&footer);

        let this = Manager(Rc::new(Inner {
            store,
            hooks,
            root,
            search,
            tree,
            model,
            footer,
            list,
            empty_title,
            empty_hint,
            edit_buttons: vec![
                connect.clone(),
                health.clone(),
                edit.clone(),
                delete.clone(),
            ],
            checks: RefCell::new(HashMap::new()),
            last_check: RefCell::new(None),
        }));

        let m = this.clone();
        this.0.search.connect_search_changed(move |_| m.refresh());
        let m = this.clone();
        this.0
            .search
            .connect_activate(move |_| m.connect_selected());
        let m = this.clone();
        this.0
            .tree
            .selection()
            .connect_changed(move |_| m.update_buttons());
        let m = this.clone();
        this.0.tree.connect_row_activated(move |tree, path, _| {
            if m.selected_id().is_some() {
                m.connect_selected();
            } else if tree.row_expanded(path) {
                tree.collapse_row(path);
            } else {
                tree.expand_row(path, false);
            }
        });
        let m = this.clone();
        this.0.tree.connect_button_press_event(move |tree, ev| {
            if ev.button() == 3 && ev.event_type() == gtk::gdk::EventType::ButtonPress {
                let (x, y) = ev.position();
                if let Some((Some(path), ..)) = tree.path_at_pos(x as i32, y as i32) {
                    tree.selection().select_path(&path);
                }
                m.context_menu(ev);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        // Typing anywhere in the list jumps to the search box.
        let m = this.clone();
        this.0.tree.connect_key_press_event(move |_, ev| {
            if let Some(c) = ev.keyval().to_unicode()
                && !c.is_control()
                && !ev.state().intersects(
                    gtk::gdk::ModifierType::CONTROL_MASK | gtk::gdk::ModifierType::MOD1_MASK,
                )
            {
                m.0.search.grab_focus_without_selecting();
                m.0.search.insert_text(&c.to_string(), &mut -1);
                m.0.search.set_position(-1);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });

        let m = this.clone();
        connect.connect_clicked(move |_| m.connect_selected());
        let m = this.clone();
        quick.connect_clicked(move |_| (m.0.hooks.quick_connect)());
        let m = this.clone();
        health.connect_clicked(move |_| m.health_selected());
        let m = this.clone();
        check.connect_clicked(move |_| (m.0.hooks.check_all)());
        let m = this.clone();
        add.connect_clicked(move |_| m.add());
        let m = this.clone();
        edit.connect_clicked(move |_| m.edit_selected());
        let m = this.clone();
        delete.connect_clicked(move |_| m.delete_selected());

        this.0.root.show_all();
        this.refresh();
        this
    }

    pub fn widget(&self) -> gtk::Widget {
        self.0.root.clone().upcast()
    }

    pub fn focus_search(&self) {
        self.0.search.grab_focus();
    }

    /// Rebuild the list from the store, keeping only rows that match the search.
    pub fn refresh(&self) {
        let selected = self.selected_id();
        let query = self.0.search.text().to_string();
        let store = self.0.store.borrow();
        let model = &self.0.model;
        model.clear();

        let mut groups: BTreeMap<String, (String, Vec<&Connection>)> = BTreeMap::new();
        let mut shown = 0;
        for c in store.connections.iter().filter(|c| c.matches(&query)) {
            groups
                .entry(c.group.to_lowercase())
                .or_insert_with(|| (c.group.clone(), Vec::new()))
                .1
                .push(c);
            shown += 1;
        }

        let mut reselect = None;
        // Named groups first, then ungrouped servers at the top level.
        let mut ordered: Vec<_> = groups.into_values().collect();
        ordered.sort_by_key(|(name, _)| (name.is_empty(), name.to_lowercase()));
        for (group, mut conns) in ordered {
            conns.sort_by_key(|c| c.display_name().to_lowercase());
            let checks = self.0.checks.borrow();
            let worst = conns
                .iter()
                .filter_map(|c| checks.get(&c.id).and_then(Check::health))
                .max();
            drop(checks);
            let parent = (!group.is_empty()).then(|| {
                let it = model.append(None);
                // Group rows only show health when something in them needs attention.
                let health = worst
                    .filter(|h| h.is_alarm())
                    .map(health_markup)
                    .unwrap_or_default();
                model.set(
                    &it,
                    &[
                        (COL_NAME, &format!("{group} ({})", conns.len())),
                        (COL_ID, &""),
                        (COL_WEIGHT, &700i32),
                        (COL_HEALTH, &health),
                    ],
                );
                it
            });
            for c in conns {
                let it = model.append(parent.as_ref());
                let first_line = c.notes.lines().next().unwrap_or_default();
                let check = self.0.checks.borrow().get(&c.id).cloned();
                let tooltip = [
                    check.as_ref().map(Check::tooltip).unwrap_or_default(),
                    c.notes.clone(),
                ]
                .iter()
                .filter(|s| !s.is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join("\n\n");
                model.set(
                    &it,
                    &[
                        (COL_NAME, &c.display_name()),
                        (COL_ADDRESS, &c.address),
                        (
                            COL_USER,
                            &if c.user.is_empty() {
                                store.default_user()
                            } else {
                                c.user.clone()
                            },
                        ),
                        (COL_TAGS, &c.tags.join(", ")),
                        (COL_SERVICE_TAG, &c.service_tag),
                        (COL_MODEL, &c.model),
                        (COL_NOTES, &first_line),
                        (COL_ID, &c.id),
                        (COL_WEIGHT, &400i32),
                        (COL_TOOLTIP, &glib::markup_escape_text(&tooltip).to_string()),
                        (
                            COL_HEALTH,
                            &check.as_ref().map(Check::markup).unwrap_or_default(),
                        ),
                    ],
                );
                if selected.as_deref() == Some(c.id.as_str()) {
                    reselect = Some(it);
                }
            }
        }
        self.0.tree.expand_all();

        // Keep the selection; otherwise pick the first match so Enter in the search box connects.
        let first_server = || {
            let it = model.iter_first()?;
            if row_id(model, &it).is_empty() {
                model.iter_children(Some(&it))
            } else {
                Some(it)
            }
        };
        if let Some(it) = reselect.or_else(|| (!query.is_empty()).then(first_server).flatten()) {
            self.0.tree.selection().select_iter(&it);
        }

        let total = store.connections.len();
        if shown == 0 {
            let (title, hint) = if total == 0 {
                (
                    "No servers yet",
                    "Click “Add” to build your address book, or “Quick Connect” for a one-off console.",
                )
            } else {
                (
                    "No matches",
                    "Nothing matches the search. Names, addresses, groups, tags, service tags / serials, models and notes are searched.",
                )
            };
            self.0.empty_title.set_text(title);
            self.0.empty_hint.set_text(hint);
            self.0.list.set_visible_child_name("empty");
        } else {
            self.0.list.set_visible_child_name("list");
        }
        let path = store::path();
        let alarms = self
            .0
            .checks
            .borrow()
            .iter()
            .filter(|(id, c)| store.get(id).is_some() && c.health().is_some_and(Health::is_alarm))
            .count();
        let mut footer = if query.is_empty() {
            format!("{total} servers")
        } else {
            format!("{shown} of {total} servers match")
        };
        if let Some(at) = self.0.last_check.borrow().as_ref() {
            footer += &match alarms {
                0 => format!(" · no alarms (checked {at})"),
                1 => format!(" · 1 server with alarms (checked {at})"),
                n => format!(" · {n} servers with alarms (checked {at})"),
            };
        }
        self.0
            .footer
            .set_text(&format!("{footer} · {}", store::display_path(&path)));
        drop(store);
        self.update_buttons();
    }

    fn selected_id(&self) -> Option<String> {
        let (model, it) = self.0.tree.selection().selected()?;
        let id = row_id(&model, &it);
        (!id.is_empty()).then_some(id)
    }

    fn selected(&self) -> Option<Connection> {
        let id = self.selected_id()?;
        self.0.store.borrow().get(&id).cloned()
    }

    fn update_buttons(&self) {
        let has = self.selected_id().is_some();
        for b in &self.0.edit_buttons {
            b.set_sensitive(has);
        }
    }

    fn export_selected(&self) {
        if let Some(c) = self.selected() {
            (self.0.hooks.export)(&c);
        }
    }

    fn health_selected(&self) {
        if let Some(c) = self.selected() {
            (self.0.hooks.health)(&c);
        }
    }

    /// Record an alarm-check result; returns the previous one.
    pub fn set_check(&self, id: &str, check: Check) -> Option<Check> {
        let previous = self.0.checks.borrow_mut().insert(id.to_string(), check);
        self.refresh();
        previous
    }

    /// Note that a round of checks finished now.
    pub fn checked_at(&self, when: String) {
        *self.0.last_check.borrow_mut() = Some(when);
        self.refresh();
    }

    fn connect_selected(&self) {
        if let Some(c) = self.selected() {
            (self.0.hooks.connect)(&c);
        }
    }

    fn parent_window(&self) -> Option<gtk::Window> {
        self.0.root.toplevel().and_then(|w| w.downcast().ok())
    }

    fn add(&self) {
        let defaults = Connection {
            id: store::new_id(),
            user: self.0.store.borrow().default_user(),
            // Pre-fill the group of the selected server, if any.
            group: self.selected().map(|c| c.group).unwrap_or_default(),
            ..Default::default()
        };
        self.edit(defaults, "Add Server");
    }

    fn edit_selected(&self) {
        if let Some(c) = self.selected() {
            self.edit(c, "Edit Server");
        }
    }

    fn duplicate_selected(&self) {
        if let Some(c) = self.selected() {
            let copy = Connection {
                id: store::new_id(),
                name: format!("{} (copy)", c.display_name()),
                save_password: false,
                ..c
            };
            self.edit(copy, "Add Server");
        }
    }

    fn edit(&self, conn: Connection, title: &str) {
        let groups = self.0.store.borrow().groups();
        let Some((mut conn, password)) =
            edit_dialog(self.parent_window().as_ref(), title, conn, &groups)
        else {
            return;
        };
        let parent = self.parent_window();
        let mut stored = match password {
            PasswordChange::Keep => Ok(()),
            PasswordChange::Set(p) => conn.set_saved_password(&p),
            PasswordChange::Forget => {
                conn.forget_password();
                Ok(())
            }
        };
        if let Err(e) = &stored
            && let Some(k) = store::keyring_unavailable(e)
        {
            // Offer to keep the entry without the password.
            if question_dialog(
                parent.as_ref(),
                k.summary(),
                &format!(
                    "{}\n\nSave the server without its password? It is asked for when connecting.",
                    k.details()
                ),
                "Save Without Password",
            ) {
                conn.save_password = false;
                stored = Ok(());
            } else {
                return;
            }
        }
        let result = stored.and_then(|()| {
            let mut store = self.0.store.borrow_mut();
            store.upsert(conn.clone());
            store.save()
        });
        if let Err(e) = result {
            error_dialog(parent.as_ref(), &format!("{e:#}"));
        }
        self.0.search.set_text("");
        self.refresh();
        self.select(&conn.id);
    }

    fn select(&self, id: &str) {
        let model = &self.0.model;
        model.foreach(|m, path, it| {
            if row_id(m, it) == id {
                self.0.tree.expand_to_path(path);
                self.0.tree.selection().select_iter(it);
                self.0.tree.scroll_to_cell(
                    Some(path),
                    None::<&gtk::TreeViewColumn>,
                    false,
                    0.0,
                    0.0,
                );
                return true;
            }
            false
        });
    }

    fn delete_selected(&self) {
        let Some(c) = self.selected() else { return };
        let dialog = gtk::MessageDialog::new(
            self.parent_window().as_ref(),
            gtk::DialogFlags::MODAL,
            gtk::MessageType::Question,
            gtk::ButtonsType::None,
            &format!("Delete “{}” ({})?", c.display_name(), c.address),
        );
        dialog.set_secondary_text(Some(
            "Its saved password is removed from the keyring as well.",
        ));
        dialog.add_buttons(&[
            ("Cancel", gtk::ResponseType::Cancel),
            ("Delete", gtk::ResponseType::Accept),
        ]);
        let response = dialog.run();
        // SAFETY: the dialog is not used after this point.
        unsafe { dialog.destroy() };
        if response != gtk::ResponseType::Accept {
            return;
        }
        c.forget_password();
        let mut store = self.0.store.borrow_mut();
        store.remove(&c.id);
        let saved = store.save();
        drop(store);
        if let Err(e) = saved {
            error_dialog(self.parent_window().as_ref(), &format!("{e:#}"));
        }
        self.refresh();
    }

    fn context_menu(&self, ev: &gtk::gdk::EventButton) {
        let menu = gtk::Menu::new();
        let has = self.selected_id().is_some();
        let item = |label: &str, enabled: bool, f: Box<dyn Fn(&Manager)>| {
            let mi = gtk::MenuItem::with_label(label);
            mi.set_sensitive(enabled);
            let m = self.clone();
            mi.connect_activate(move |_| f(&m));
            menu.append(&mi);
        };
        item("Connect", has, Box::new(|m| m.connect_selected()));
        item("Health & SEL", has, Box::new(|m| m.health_selected()));
        item("Export Logs…", has, Box::new(|m| m.export_selected()));
        item("Edit…", has, Box::new(|m| m.edit_selected()));
        item("Duplicate…", has, Box::new(|m| m.duplicate_selected()));
        item("Delete…", has, Box::new(|m| m.delete_selected()));
        menu.append(&gtk::SeparatorMenuItem::new());
        item("Add Server…", true, Box::new(|m| m.add()));
        menu.show_all();
        menu.popup_at_pointer(Some(ev));
    }
}

/// The connection id of a row; empty for group rows.
fn row_id(model: &impl IsA<gtk::TreeModel>, it: &gtk::TreeIter) -> String {
    model
        .value(it, COL_ID as i32)
        .get::<String>()
        .unwrap_or_default()
}

pub enum PasswordChange {
    Keep,
    Set(String),
    Forget,
}

fn edit_dialog(
    parent: Option<&gtk::Window>,
    title: &str,
    conn: Connection,
    groups: &[String],
) -> Option<(Connection, PasswordChange)> {
    let dialog = gtk::Dialog::with_buttons(
        Some(title),
        parent,
        gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
        &[
            ("Cancel", gtk::ResponseType::Cancel),
            ("Save", gtk::ResponseType::Ok),
        ],
    );
    dialog.set_default_response(gtk::ResponseType::Ok);
    dialog.set_default_size(520, -1);

    let grid = gtk::Grid::builder()
        .row_spacing(8)
        .column_spacing(12)
        .margin(16)
        .build();
    let mut row = 0;
    let mut attach = |label: &str, widget: &gtk::Widget| {
        let l = gtk::Label::builder()
            .label(label)
            .xalign(1.0)
            .yalign(0.0)
            .margin_top(6)
            .build();
        grid.attach(&l, 0, row, 1, 1);
        widget.set_hexpand(true);
        grid.attach(widget, 1, row, 1, 1);
        row += 1;
    };
    let entry = |text: &str, placeholder: &str| {
        gtk::Entry::builder()
            .text(text)
            .placeholder_text(placeholder)
            .activates_default(true)
            .build()
    };

    let name = entry(&conn.name, "defaults to the address");
    let address = entry(
        &conn.address,
        "hostname or IP of the BMC (iDRAC, Supermicro or MegaRAC)",
    );
    let vendor = gtk::ComboBoxText::new();
    vendor.append(Some("auto"), "Detect on connect");
    for v in Vendor::ALL {
        vendor.append(Some(v.id()), v.label());
    }
    vendor.set_active_id(Some(conn.vendor.map(Vendor::id).unwrap_or("auto")));
    let user = entry(
        &conn.user,
        "root (Dell), ADMIN (Supermicro) or admin (MegaRAC)",
    );
    let password = entry(
        "",
        if conn.save_password {
            "saved — leave blank to keep"
        } else {
            "asked when connecting unless saved"
        },
    );
    password.set_visibility(false);
    password.set_input_purpose(gtk::InputPurpose::Password);
    let save_password = gtk::CheckButton::with_label("Save password in the system keyring");
    save_password.set_active(conn.save_password);

    let group = gtk::ComboBoxText::with_entry();
    for g in groups {
        group.append_text(g);
    }
    if let Some(e) = group.child().and_then(|c| c.downcast::<gtk::Entry>().ok()) {
        e.set_text(&conn.group);
        e.set_placeholder_text(Some("e.g. Lab, Japan DC"));
        e.set_activates_default(true);
    }
    let tags = entry(&conn.tags.join(", "), "comma separated, e.g. r630, pxe");
    let service_tag = entry(&conn.service_tag, "filled in on first connect");
    let model = entry(&conn.model, "filled in on first connect");
    let notes = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .accepts_tab(false)
        .left_margin(6)
        .right_margin(6)
        .top_margin(4)
        .bottom_margin(4)
        .build();
    notes.buffer().expect("text buffer").set_text(&conn.notes);
    let notes_scroll = gtk::ScrolledWindow::builder()
        .shadow_type(gtk::ShadowType::In)
        .min_content_height(100)
        .build();
    notes_scroll.add(&notes);

    attach("Name", name.upcast_ref());
    attach("Address", address.upcast_ref());
    attach("BMC type", vendor.upcast_ref());
    attach("User", user.upcast_ref());
    attach("Password", password.upcast_ref());
    attach("", save_password.upcast_ref());
    attach("Group", group.upcast_ref());
    attach("Tags", tags.upcast_ref());
    attach("Service Tag / Serial", service_tag.upcast_ref());
    attach("Model", model.upcast_ref());
    attach("Notes", notes_scroll.upcast_ref());
    dialog.content_area().add(&grid);
    dialog.show_all();
    if conn.address.is_empty() {
        address.grab_focus();
    }

    let result = loop {
        if dialog.run() != gtk::ResponseType::Ok {
            break None;
        }
        let addr = address.text().trim().to_string();
        if addr.is_empty() {
            address.grab_focus();
            continue;
        }
        let buffer = notes.buffer().expect("text buffer");
        let (start, end) = buffer.bounds();
        let new_password = password.text().to_string();
        let save = save_password.is_active();
        // "Save password" ticked with a blank field keeps the saved one, or (for a new entry)
        // asks for it on connect and saves it then.
        let change = match (save, new_password.is_empty()) {
            (false, _) => PasswordChange::Forget,
            (true, false) => PasswordChange::Set(new_password),
            (true, true) => PasswordChange::Keep,
        };
        let updated = Connection {
            id: conn.id.clone(),
            name: name.text().trim().to_string(),
            address: addr,
            user: user.text().trim().to_string(),
            group: group
                .active_text()
                .map(|g| g.trim().to_string())
                .unwrap_or_default(),
            tags: tags
                .text()
                .split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect(),
            service_tag: service_tag.text().trim().to_string(),
            model: model.text().trim().to_string(),
            vendor: vendor.active_id().as_deref().and_then(Vendor::parse),
            notes: buffer
                .text(&start, &end, false)
                .map(|t| t.trim_end().to_string())
                .unwrap_or_default(),
            save_password: save,
        };
        break Some((updated, change));
    };
    // SAFETY: the dialog is not used after this point.
    unsafe { dialog.destroy() };
    result
}

/// A yes/no question with an explanation; `true` when the user chose `yes_label`.
pub fn question_dialog(
    parent: Option<&gtk::Window>,
    msg: &str,
    question: &str,
    yes_label: &str,
) -> bool {
    let dialog = gtk::MessageDialog::new(
        parent,
        gtk::DialogFlags::MODAL,
        gtk::MessageType::Warning,
        gtk::ButtonsType::None,
        msg,
    );
    dialog.set_secondary_text(Some(question));
    dialog.set_title("Rust Drac");
    dialog.add_buttons(&[
        ("Cancel", gtk::ResponseType::Cancel),
        (yes_label, gtk::ResponseType::Accept),
    ]);
    dialog.set_default_response(gtk::ResponseType::Accept);
    let answer = dialog.run() == gtk::ResponseType::Accept;
    // SAFETY: the dialog is not used after this point.
    unsafe { dialog.destroy() };
    answer
}

pub fn error_dialog(parent: Option<&gtk::Window>, msg: &str) {
    let dialog = gtk::MessageDialog::new(
        parent,
        gtk::DialogFlags::MODAL,
        gtk::MessageType::Error,
        gtk::ButtonsType::Close,
        msg,
    );
    dialog.set_title("Rust Drac");
    dialog.run();
    // SAFETY: the dialog is not used after this point.
    unsafe { dialog.destroy() };
}
