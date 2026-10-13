//! The home page: saved connections in colourful, reorderable groups.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use super::{connection_dialog, group_dialog, icons};
use crate::config::{self, ConnectionConfig, ConnectionDrop, Group, Store};
use crate::db::DbKind;
use crate::{runtime, secrets};

type ConnectFn = Box<dyn Fn(ConnectionConfig, Box<dyn FnOnce()>)>;

/// What a list row represents.
#[derive(Clone)]
enum RowKind {
    /// A group header; `None` is the "Ungrouped" section.
    Group(Option<String>),
    Connection(String),
}

const DROP_CLASSES: [&str; 3] = ["drop-before", "drop-after", "drop-into"];

/// The "⋮" button on a row; hidden until hover, but kept visible while its menu is open.
fn row_menu_button(model: &gio::Menu, tooltip: &str) -> gtk::MenuButton {
    let button = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .menu_model(model)
        .valign(gtk::Align::Center)
        .tooltip_text(tooltip)
        .css_classes(["flat", "circular", "row-menu"])
        .build();
    button.connect_active_notify(|b| {
        if b.is_active() {
            b.add_css_class("open");
        } else {
            b.remove_css_class("open");
        }
    });
    button
}

pub struct Welcome {
    pub widget: adw::ToolbarView,
    /// Asked to open a connection; the callback runs when the attempt has finished.
    on_connect: RefCell<Option<ConnectFn>>,
    list: gtk::ListBox,
    stack: gtk::Stack,
    search: gtk::SearchEntry,
    /// "4 connections in 2 groups" under the page heading.
    summary: gtk::Label,
    toasts: adw::ToastOverlay,
    actions: gio::SimpleActionGroup,
    store: RefCell<Store>,
    rows: RefCell<Vec<RowKind>>,
    connecting: RefCell<HashSet<String>>,
}

impl Welcome {
    pub fn new() -> Rc<Self> {
        // ---- Left panel ----
        let logo = gtk::Image::builder()
            .icon_name(crate::APP_ID)
            .pixel_size(56)
            .css_classes(["home-logo"])
            .build();
        let title = gtk::Label::builder().label("Pueo").xalign(0.0).css_classes(["title-2"]).build();
        let version = gtk::Label::builder()
            .label(format!("v{}", env!("CARGO_PKG_VERSION")))
            .halign(gtk::Align::Start)
            .css_classes(["home-version"])
            .build();
        let brand_text = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .valign(gtk::Align::Center)
            .build();
        brand_text.append(&title);
        brand_text.append(&version);
        let brand = gtk::Box::builder().spacing(12).css_classes(["home-brand"]).build();
        brand.append(&logo);
        brand.append(&brand_text);

        // Primary action: a filled button with its shortcut as a keycap.
        let create_content = gtk::Box::builder().spacing(8).build();
        create_content.append(&gtk::Image::from_icon_name("list-add-symbolic"));
        create_content.append(&gtk::Label::builder().label("New Connection").hexpand(true).xalign(0.0).build());
        create_content.append(&gtk::Label::builder().label("Ctrl N").css_classes(["keycap-hint"]).build());
        let create = gtk::Button::builder()
            .child(&create_content)
            .action_name("win.new-connection")
            .css_classes(["suggested-action", "home-primary"])
            .build();

        let side_button = |icon: &str, label: &str| {
            gtk::Button::builder()
                .child(&adw::ButtonContent::builder().icon_name(icon).label(label).halign(gtk::Align::Start).build())
                .css_classes(["flat", "home-action"])
                .build()
        };
        let new_group = side_button("folder-new-symbolic", "New Group");
        let open_file = side_button("document-open-symbolic", "Open SQLite File…");

        let side = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .width_request(260)
            .hexpand(false)
            .css_classes(["home-side-content"])
            .build();
        side.append(&brand);
        side.append(&gtk::Box::builder().height_request(24).build());
        side.append(&create);
        side.append(&gtk::Box::builder().height_request(8).build());
        side.append(&new_group);
        side.append(&open_file);

        // ---- Right: heading, search and grouped list ----
        let heading = gtk::Label::builder().label("Connections").xalign(0.0).css_classes(["title-1"]).build();
        let summary = gtk::Label::builder().xalign(0.0).css_classes(["dim-label"]).build();
        let heading_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).build();
        heading_box.append(&heading);
        heading_box.append(&summary);

        let search = gtk::SearchEntry::builder()
            .placeholder_text("Search by name, host, type or tag…")
            .margin_top(14)
            .margin_bottom(4)
            .css_classes(["home-search"])
            .build();
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["home-list"])
            .build();
        let list_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_top(28)
            .margin_bottom(32)
            .margin_start(24)
            .margin_end(24)
            .build();
        list_box.append(&heading_box);
        list_box.append(&search);
        list_box.append(&list);
        let clamp = adw::Clamp::builder().maximum_size(760).child(&list_box).build();
        let scroller = gtk::ScrolledWindow::builder()
            .child(&clamp)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .hexpand(true)
            .build();

        let empty_create = gtk::Button::builder()
            .label("Create Connection")
            .halign(gtk::Align::Center)
            .action_name("win.new-connection")
            .css_classes(["pill", "suggested-action"])
            .build();
        let empty = adw::StatusPage::builder()
            .icon_name("network-server-symbolic")
            .title("No Connections Yet")
            .description("Connect to PostgreSQL, MySQL, SQLite or Redis")
            .child(&empty_create)
            .hexpand(true)
            .build();

        let stack = gtk::Stack::new();
        stack.add_named(&empty, Some("empty"));
        stack.add_named(&scroller, Some("list"));

        let menu = gio::Menu::new();
        menu.append(Some("_Keyboard Shortcuts"), Some("app.shortcuts"));
        menu.append(Some("_About Pueo"), Some("app.about"));
        let menu_button = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(&menu)
            .primary(true)
            .build();
        // Each pane has its own header bar so the side panel's tint runs to the top edge.
        // The page has its own large heading, so neither shows a title.
        let side_header = adw::HeaderBar::builder().show_title(false).show_end_title_buttons(false).build();
        let side_toolbar = adw::ToolbarView::builder().css_classes(["home-side"]).build();
        side_toolbar.add_top_bar(&side_header);
        side_toolbar.set_content(Some(&side));

        let header = adw::HeaderBar::builder().show_title(false).show_start_title_buttons(false).build();
        header.pack_end(&menu_button);
        let content_toolbar = adw::ToolbarView::builder().hexpand(true).build();
        content_toolbar.add_top_bar(&header);
        content_toolbar.set_content(Some(&stack));

        let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        body.append(&side_toolbar);
        body.append(&content_toolbar);

        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&body));
        let toolbar = adw::ToolbarView::new();
        toolbar.set_content(Some(&toasts));

        let welcome = Rc::new(Welcome {
            widget: toolbar,
            on_connect: RefCell::new(None),
            list,
            stack,
            search,
            summary,
            toasts,
            actions: gio::SimpleActionGroup::new(),
            store: RefCell::new(config::load_store()),
            rows: RefCell::new(Vec::new()),
            connecting: RefCell::new(HashSet::new()),
        });
        welcome.setup_actions();

        let weak = Rc::downgrade(&welcome);
        new_group.connect_clicked(move |_| {
            if let Some(w) = weak.upgrade() {
                w.edit_group(None);
            }
        });
        let weak = Rc::downgrade(&welcome);
        open_file.connect_clicked(move |_| {
            if let Some(w) = weak.upgrade() {
                w.open_sqlite_file();
            }
        });
        let weak = Rc::downgrade(&welcome);
        welcome.search.connect_search_changed(move |_| {
            if let Some(w) = weak.upgrade() {
                w.rebuild();
            }
        });
        let weak = Rc::downgrade(&welcome);
        welcome.list.connect_row_activated(move |_, row| {
            let Some(w) = weak.upgrade() else { return };
            let kind = w.rows.borrow().get(row.index() as usize).cloned();
            match kind {
                Some(RowKind::Connection(id)) => w.connect(&id),
                Some(RowKind::Group(Some(id))) => w.toggle_group(&id),
                _ => {}
            }
        });

        welcome.rebuild();
        welcome
    }

    pub fn connect_open(&self, f: impl Fn(ConnectionConfig, Box<dyn FnOnce()>) + 'static) {
        *self.on_connect.borrow_mut() = Some(Box::new(f));
    }

    pub fn new_connection(self: &Rc<Self>) {
        self.edit(None, None);
    }

    pub fn focus_search(&self) {
        self.search.grab_focus();
    }

    // ---------- actions (targets are connection / group ids) ----------

    fn setup_actions(self: &Rc<Self>) {
        let add = |name: &str, f: fn(&Rc<Self>, &str)| {
            let action = gio::SimpleAction::new(name, Some(glib::VariantTy::STRING));
            let weak = Rc::downgrade(self);
            action.connect_activate(move |_, param| {
                if let (Some(w), Some(arg)) = (weak.upgrade(), param.and_then(|p| p.str().map(str::to_owned))) {
                    f(&w, &arg);
                }
            });
            self.actions.add_action(&action);
        };
        add("connect", |w, id| w.connect(id));
        add("edit-conn", |w, id| {
            let cfg = w.connection(id);
            w.edit(cfg, None);
        });
        add("duplicate", |w, id| w.duplicate(id));
        add("delete-conn", |w, id| w.confirm_delete(id));
        // "connection-id|group-id" (empty group id = ungrouped)
        add("move-to", |w, arg| {
            if let Some((id, group)) = arg.split_once('|') {
                let group = (!group.is_empty()).then(|| group.to_string());
                w.store.borrow_mut().move_connection(id, &ConnectionDrop::IntoGroup(group));
                w.persist_and_rebuild();
            }
        });
        add("edit-group", |w, id| {
            let group = w.store.borrow().groups.iter().find(|g| g.id == id).cloned();
            w.edit_group(group);
        });
        add("new-in-group", |w, id| w.edit(None, Some(id.to_string())));
        add("delete-group", |w, id| w.confirm_delete_group(id));
        self.widget.insert_action_group("home", Some(&self.actions));
    }

    fn connection(&self, id: &str) -> Option<ConnectionConfig> {
        self.store.borrow().connections.iter().find(|c| c.id == id).cloned()
    }

    fn connection_menu(&self, id: &str) -> gio::Menu {
        let menu = gio::Menu::new();
        let item = |label: &str, action: &str, target: &str| {
            let item = gio::MenuItem::new(Some(label), None);
            item.set_action_and_target_value(Some(action), Some(&target.to_variant()));
            item
        };
        let main = gio::Menu::new();
        main.append_item(&item("Connect", "home.connect", id));
        main.append_item(&item("Edit…", "home.edit-conn", id));
        main.append_item(&item("Duplicate", "home.duplicate", id));
        menu.append_section(None, &main);

        let groups = gio::Menu::new();
        let current = self.connection(id).and_then(|c| c.group);
        if current.is_some() {
            groups.append_item(&item("Ungrouped", "home.move-to", &format!("{id}|")));
        }
        for g in self.store.borrow().groups.iter().filter(|g| Some(&g.id) != current.as_ref()) {
            groups.append_item(&item(&g.name, "home.move-to", &format!("{id}|{}", g.id)));
        }
        if groups.n_items() > 0 {
            menu.append_submenu(Some("Move to Group"), &groups);
        }
        let danger = gio::Menu::new();
        danger.append_item(&item("Delete…", "home.delete-conn", id));
        menu.append_section(None, &danger);
        menu
    }

    fn group_menu(&self, id: &str) -> gio::Menu {
        let menu = gio::Menu::new();
        let item = |label: &str, action: &str| {
            let item = gio::MenuItem::new(Some(label), None);
            item.set_action_and_target_value(Some(action), Some(&id.to_variant()));
            item
        };
        let main = gio::Menu::new();
        main.append_item(&item("New Connection Here…", "home.new-in-group"));
        main.append_item(&item("Edit Group…", "home.edit-group"));
        menu.append_section(None, &main);
        let danger = gio::Menu::new();
        danger.append_item(&item("Delete Group…", "home.delete-group"));
        menu.append_section(None, &danger);
        menu
    }

    // ---------- building the list ----------

    fn matches(cfg: &ConnectionConfig, needle: &str) -> bool {
        needle.is_empty()
            || cfg.display_name().to_lowercase().contains(needle)
            || cfg.summary().to_lowercase().contains(needle)
            || cfg.kind.label().to_lowercase().contains(needle)
            || cfg.tag.is_some_and(|t| t.label().to_lowercase().contains(needle))
    }

    fn rebuild(self: &Rc<Self>) {
        self.list.remove_all();
        let mut rows: Vec<(RowKind, gtk::ListBoxRow)> = Vec::new();
        let store = self.store.borrow().clone();
        let needle = self.search.text().to_lowercase();
        let searching = !needle.is_empty();
        let has_groups = !store.groups.is_empty();

        for group in &store.groups {
            let members: Vec<&ConnectionConfig> = store
                .connections
                .iter()
                .filter(|c| c.group.as_deref() == Some(&group.id))
                .collect();
            let shown: Vec<&&ConnectionConfig> = members
                .iter()
                .filter(|c| Self::matches(c, &needle) || group.name.to_lowercase().contains(&needle))
                .collect();
            if searching && shown.is_empty() {
                continue;
            }
            let expanded = searching || !group.collapsed;
            rows.push((RowKind::Group(Some(group.id.clone())), self.group_row(Some(group), members.len(), expanded)));
            if expanded {
                for cfg in shown {
                    rows.push((RowKind::Connection(cfg.id.clone()), self.connection_row(cfg)));
                }
            }
        }

        let ungrouped: Vec<&ConnectionConfig> =
            store.connections.iter().filter(|c| c.group.is_none() && Self::matches(c, &needle)).collect();
        // The "Ungrouped" header doubles as the drop target for leaving a group.
        if has_groups && (!ungrouped.is_empty() || !searching) {
            let total = store.connections.iter().filter(|c| c.group.is_none()).count();
            rows.push((RowKind::Group(None), self.group_row(None, total, true)));
        }
        for cfg in ungrouped {
            rows.push((RowKind::Connection(cfg.id.clone()), self.connection_row(cfg)));
        }

        // Each run of connection rows forms one card: round its outer corners.
        let is_conn = |i: usize| matches!(rows.get(i), Some((RowKind::Connection(_), _)));
        for (i, (_, row)) in rows.iter().enumerate() {
            if is_conn(i) {
                if i == 0 || !is_conn(i - 1) {
                    row.add_css_class("card-first");
                }
                if !is_conn(i + 1) {
                    row.add_css_class("card-last");
                }
            }
            self.list.append(row);
        }
        *self.rows.borrow_mut() = rows.into_iter().map(|(kind, _)| kind).collect();

        let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
        let mut summary = plural(store.connections.len(), "connection");
        if has_groups {
            summary.push_str(&format!(" in {}", plural(store.groups.len(), "group")));
        }
        self.summary.set_label(&summary);

        let empty = store.connections.is_empty() && store.groups.is_empty();
        self.stack.set_visible_child_name(if empty { "empty" } else { "list" });
    }

    fn group_row(self: &Rc<Self>, group: Option<&Group>, count: usize, expanded: bool) -> gtk::ListBoxRow {
        let content = gtk::Box::builder().spacing(12).build();
        match group {
            Some(g) => {
                content.append(&icons::group_icon(&g.icon, g.color, 28));
                content.append(&gtk::Label::builder().label(&g.name).xalign(0.0).css_classes(["heading"]).build());
            }
            None => {
                content.append(
                    &gtk::Label::builder().label("Ungrouped").xalign(0.0).css_classes(["heading", "group-plain"]).build(),
                );
            }
        }
        content.append(
            &gtk::Label::builder()
                .label(count.to_string())
                .css_classes(["group-count"])
                .valign(gtk::Align::Center)
                .build(),
        );
        content.append(&gtk::Box::builder().hexpand(true).build());

        let row = gtk::ListBoxRow::builder()
            .child(&content)
            .activatable(group.is_some())
            .css_classes(["group-row"])
            .build();

        if let Some(g) = group {
            row.set_cursor_from_name(Some("pointer"));
            content.append(&row_menu_button(&self.group_menu(&g.id), "Group Options"));
            let chevron = gtk::Image::builder().icon_name("pan-down-symbolic").css_classes(["group-chevron"]).build();
            if !expanded {
                chevron.add_css_class("collapsed");
            }
            content.append(&chevron);
            self.add_context_menu(&row, self.group_menu(&g.id));
            self.add_drag_source(&row, format!("group:{}", g.id));
        }
        self.add_drop_target(&row, RowKind::Group(group.map(|g| g.id.clone())));
        row
    }

    fn connection_row(self: &Rc<Self>, cfg: &ConnectionConfig) -> gtk::ListBoxRow {
        // The database logo as a large, tilted, faint watermark behind the row's end.
        let watermark = gtk::Image::builder()
            .icon_name(cfg.kind.icon_name())
            .pixel_size(76)
            .halign(gtk::Align::End)
            .valign(gtk::Align::Center)
            .margin_end(48)
            // Nudges the (rotated) logo a few pixels below centre, where it looks centred.
            .margin_top(6)
            .can_target(false)
            .css_classes(["conn-watermark", cfg.kind.css_class()])
            .build();

        let name = gtk::Label::builder()
            .label(cfg.display_name())
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["conn-name"])
            .build();
        let title = gtk::Box::builder().spacing(8).build();
        title.append(&name);
        if let Some(tag) = cfg.tag {
            title.append(
                &gtk::Label::builder()
                    .label(tag.short())
                    .valign(gtk::Align::Center)
                    .css_classes(["tag-pill", &format!("solid-{}", tag.color().name())])
                    .build(),
            );
        }
        let subtitle = gtk::Label::builder()
            .label(format!("{} · {}", cfg.kind.label(), cfg.summary()))
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .css_classes(["dim-label", "caption"])
            .build();
        let text = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        text.append(&title);
        text.append(&subtitle);

        let content = gtk::Box::builder().spacing(12).build();
        content.append(&text);
        if self.connecting.borrow().contains(&cfg.id) {
            content.append(&adw::Spinner::new());
        }
        content.append(&row_menu_button(&self.connection_menu(&cfg.id), "Connection Options"));
        content.append(&gtk::Image::builder().icon_name("go-next-symbolic").css_classes(["conn-go"]).build());

        // Layers, bottom to top: the watermark (drawn at full size so it stays sharp, and
        // left out of the row's height so it just bleeds off the edges), then the content,
        // which alone sets the row's size.
        let layers = gtk::Overlay::builder().child(&gtk::Box::new(gtk::Orientation::Horizontal, 0)).build();
        layers.add_overlay(&watermark);
        layers.add_overlay(&content);
        layers.set_measure_overlay(&content, true);

        let row = gtk::ListBoxRow::builder()
            .child(&layers)
            .overflow(gtk::Overflow::Hidden)
            .css_classes(["conn-row"])
            .tooltip_text(format!("{} · {}", cfg.kind.label(), cfg.summary()))
            .build();
        row.set_cursor_from_name(Some("pointer"));
        if let Some(color) = cfg.effective_color() {
            row.add_css_class(&format!("tint-{}", color.name()));
        }
        self.add_context_menu(&row, self.connection_menu(&cfg.id));
        self.add_drag_source(&row, format!("conn:{}", cfg.id));
        self.add_drop_target(&row, RowKind::Connection(cfg.id.clone()));
        row
    }

    fn add_context_menu(&self, row: &gtk::ListBoxRow, model: gio::Menu) {
        let click = gtk::GestureClick::builder().button(gdk::BUTTON_SECONDARY).build();
        click.connect_pressed(move |gesture, _, x, y| {
            let Some(widget) = gesture.widget() else { return };
            gesture.set_state(gtk::EventSequenceState::Claimed);
            let menu = gtk::PopoverMenu::from_model(Some(&model));
            menu.set_parent(&widget);
            menu.set_has_arrow(false);
            menu.set_halign(gtk::Align::Start);
            menu.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            menu.connect_closed(|m| {
                let m = m.clone();
                glib::idle_add_local_once(move || m.unparent());
            });
            menu.popup();
        });
        row.add_controller(click);
    }

    // ---------- drag & drop ----------

    fn add_drag_source(&self, row: &gtk::ListBoxRow, payload: String) {
        let source = gtk::DragSource::builder().actions(gdk::DragAction::MOVE).build();
        source.connect_prepare(move |_, _, _| Some(gdk::ContentProvider::for_value(&payload.to_value())));
        source.connect_drag_begin(|source, _| {
            if let Some(widget) = source.widget() {
                let paintable = gtk::WidgetPaintable::new(Some(&widget));
                source.set_icon(Some(&paintable), 24, 16);
                widget.add_css_class("dragging");
            }
        });
        source.connect_drag_end(|source, _, _| {
            if let Some(widget) = source.widget() {
                widget.remove_css_class("dragging");
            }
        });
        row.add_controller(source);
    }

    fn add_drop_target(self: &Rc<Self>, row: &gtk::ListBoxRow, kind: RowKind) {
        let target = gtk::DropTarget::new(glib::Type::STRING, gdk::DragAction::MOVE);
        // Needed so `value()` is available while hovering, to decide where the drop goes.
        target.set_preload(true);
        let clear = |w: &gtk::Widget| {
            for c in DROP_CLASSES {
                w.remove_css_class(c);
            }
        };

        // Decide what a drop at height `y` would do; returns the CSS class to show.
        let placement = {
            let kind = kind.clone();
            move |payload: &str, widget: &gtk::Widget, y: f64| -> Option<&'static str> {
                let upper = y < widget.height() as f64 / 2.0;
                match (&kind, payload.split_once(':')) {
                    (RowKind::Connection(id), Some(("conn", dragged))) if id != dragged => {
                        Some(if upper { "drop-before" } else { "drop-after" })
                    }
                    (RowKind::Group(_), Some(("conn", _))) => Some("drop-into"),
                    (RowKind::Group(Some(id)), Some(("group", dragged))) if id != dragged => {
                        Some(if upper { "drop-before" } else { "drop-after" })
                    }
                    _ => None,
                }
            }
        };

        let p = placement.clone();
        target.connect_motion(move |target, _, y| {
            let Some(widget) = target.widget() else { return gdk::DragAction::empty() };
            clear(&widget);
            let payload = target.value().and_then(|v| v.get::<String>().ok()).unwrap_or_default();
            match p(&payload, &widget, y) {
                Some(class) => {
                    widget.add_css_class(class);
                    gdk::DragAction::MOVE
                }
                None => gdk::DragAction::empty(),
            }
        });
        target.connect_leave(move |target| {
            if let Some(widget) = target.widget() {
                clear(&widget);
            }
        });

        let weak = Rc::downgrade(self);
        target.connect_drop(move |target, value, _, y| {
            let Some(widget) = target.widget() else { return false };
            clear(&widget);
            let (Some(w), Ok(payload)) = (weak.upgrade(), value.get::<String>()) else { return false };
            let Some(class) = placement(&payload, &widget, y) else { return false };
            let after = class == "drop-after";
            let Some((what, dragged)) = payload.split_once(':') else { return false };
            {
                let mut store = w.store.borrow_mut();
                match (&kind, what) {
                    (RowKind::Connection(id), "conn") => {
                        let drop = if after { ConnectionDrop::After(id.clone()) } else { ConnectionDrop::Before(id.clone()) };
                        store.move_connection(dragged, &drop);
                    }
                    (RowKind::Group(group), "conn") => {
                        store.move_connection(dragged, &ConnectionDrop::IntoGroup(group.clone()));
                        // Open a collapsed group so the dropped item stays visible.
                        if let Some(g) = store.groups.iter_mut().find(|g| Some(&g.id) == group.as_ref()) {
                            g.collapsed = false;
                        }
                    }
                    (RowKind::Group(Some(id)), "group") => store.move_group(dragged, id, after),
                    _ => return false,
                }
            }
            // Rebuilding destroys this row; do it after the drop finishes.
            let weak = Rc::downgrade(&w);
            glib::idle_add_local_once(move || {
                if let Some(w) = weak.upgrade() {
                    w.persist_and_rebuild();
                }
            });
            true
        });
        row.add_controller(target);
    }

    // ---------- operations ----------

    fn toggle_group(self: &Rc<Self>, id: &str) {
        if let Some(g) = self.store.borrow_mut().groups.iter_mut().find(|g| g.id == id) {
            g.collapsed = !g.collapsed;
        }
        self.persist_and_rebuild();
    }

    fn persist_and_rebuild(self: &Rc<Self>) {
        self.persist();
        self.rebuild();
    }

    fn edit(self: &Rc<Self>, existing: Option<ConnectionConfig>, group: Option<String>) {
        let groups = self.store.borrow().groups.clone();
        let is_new = existing.is_none();
        let existing = existing.or_else(|| {
            group.map(|g| {
                let mut cfg = ConnectionConfig::new(DbKind::Postgres);
                cfg.group = Some(g);
                cfg
            })
        });
        let weak = Rc::downgrade(self);
        connection_dialog::show(&self.widget, existing, is_new, &groups, move |cfg, password, ssh_password| {
            let Some(w) = weak.upgrade() else { return };
            {
                let mut store = w.store.borrow_mut();
                match store.connections.iter_mut().find(|c| c.id == cfg.id) {
                    Some(c) => {
                        let group_changed = c.group != cfg.group;
                        *c = cfg.clone();
                        if group_changed {
                            store.move_connection(&cfg.id, &ConnectionDrop::IntoGroup(cfg.group.clone()));
                        }
                    }
                    None => store.connections.push(cfg.clone()),
                }
            }
            w.persist_and_rebuild();

            let weak = Rc::downgrade(&w);
            let label = cfg.display_name();
            runtime::spawn(
                async move {
                    secrets::set_password(&cfg.id, secrets::Slot::Database, &label, &password).await?;
                    secrets::set_password(&cfg.id, secrets::Slot::Ssh, &label, &ssh_password).await
                },
                move |res| {
                    if let (Some(w), Err(e)) = (weak.upgrade(), res) {
                        w.toast(&format!("Password not saved to keyring: {e}"));
                    }
                },
            );
        });
    }

    fn edit_group(self: &Rc<Self>, existing: Option<Group>) {
        let weak = Rc::downgrade(self);
        group_dialog::show(&self.widget, existing, move |group| {
            let Some(w) = weak.upgrade() else { return };
            {
                let mut store = w.store.borrow_mut();
                match store.groups.iter_mut().find(|g| g.id == group.id) {
                    Some(g) => *g = group,
                    None => store.groups.push(group),
                }
            }
            w.persist_and_rebuild();
        });
    }

    fn duplicate(self: &Rc<Self>, id: &str) {
        let Some(mut copy) = self.connection(id) else { return };
        let original_id = copy.id.clone();
        copy.id = uuid::Uuid::new_v4().to_string();
        copy.name = format!("{} (copy)", copy.display_name());
        {
            let mut store = self.store.borrow_mut();
            store.connections.push(copy.clone());
            store.move_connection(&copy.id, &ConnectionDrop::After(original_id.clone()));
        }
        self.persist_and_rebuild();
        // Copy the saved passwords too.
        let label = copy.display_name();
        runtime::spawn(
            async move {
                for slot in [secrets::Slot::Database, secrets::Slot::Ssh] {
                    if let Some(pw) = secrets::get_password(&original_id, slot).await? {
                        secrets::set_password(&copy.id, slot, &label, &pw).await?;
                    }
                }
                anyhow::Ok(())
            },
            |_| {},
        );
    }

    fn confirm_delete(self: &Rc<Self>, id: &str) {
        let Some(cfg) = self.connection(id) else { return };
        let dialog = adw::AlertDialog::new(
            Some("Delete Connection?"),
            Some(&format!("“{}” and its saved password will be removed.", cfg.display_name())),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        let weak = Rc::downgrade(self);
        dialog.connect_response(None, move |_, response| {
            if response != "delete" {
                return;
            }
            let Some(w) = weak.upgrade() else { return };
            w.store.borrow_mut().connections.retain(|c| c.id != cfg.id);
            w.persist_and_rebuild();
            let id = cfg.id.clone();
            runtime::spawn(async move { secrets::delete_passwords(&id).await }, |_| {});
        });
        dialog.present(Some(&self.widget));
    }

    fn confirm_delete_group(self: &Rc<Self>, id: &str) {
        let Some(group) = self.store.borrow().groups.iter().find(|g| g.id == id).cloned() else { return };
        let dialog = adw::AlertDialog::new(
            Some("Delete Group?"),
            Some(&format!("“{}” will be removed. Its connections are kept and moved to Ungrouped.", group.name)),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        let weak = Rc::downgrade(self);
        dialog.connect_response(None, move |_, response| {
            if let (Some(w), "delete") = (weak.upgrade(), response) {
                w.store.borrow_mut().delete_group(&group.id);
                w.persist_and_rebuild();
            }
        });
        dialog.present(Some(&self.widget));
    }

    fn open_sqlite_file(self: &Rc<Self>) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("SQLite databases"));
        for pat in ["*.db", "*.sqlite", "*.sqlite3", "*.db3"] {
            filter.add_pattern(pat);
        }
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let chooser = gtk::FileDialog::builder().title("Open SQLite Database").filters(&filters).build();
        let window = self.widget.root().and_downcast::<gtk::Window>();
        let weak = Rc::downgrade(self);
        chooser.open(window.as_ref(), None::<&gio::Cancellable>, move |res| {
            let (Some(w), Ok(file)) = (weak.upgrade(), res) else { return };
            let Some(path) = file.path() else { return };
            let mut cfg = ConnectionConfig::new(DbKind::Sqlite);
            cfg.file_path = path.to_string_lossy().into_owned();
            if let Some(f) = w.on_connect.borrow().as_ref() {
                f(cfg, Box::new(|| {}));
            }
        });
    }

    fn connect(self: &Rc<Self>, id: &str) {
        let Some(cfg) = self.connection(id) else { return };
        if !self.connecting.borrow_mut().insert(id.to_string()) {
            return;
        }
        self.rebuild();
        let weak = Rc::downgrade(self);
        let id = id.to_string();
        let done = Box::new(move || {
            if let Some(w) = weak.upgrade() {
                w.connecting.borrow_mut().remove(&id);
                w.rebuild();
            }
        });
        match self.on_connect.borrow().as_ref() {
            Some(f) => f(cfg, done),
            None => done(),
        }
    }

    fn persist(&self) {
        if let Err(e) = config::save_store(&self.store.borrow()) {
            self.toast(&format!("Could not save connections: {e}"));
        }
    }

    fn toast(&self, msg: &str) {
        self.toasts.add_toast(adw::Toast::new(&glib::markup_escape_text(msg)));
    }
}
