//! The object tree in the sidebar: collapsible sections (Functions, Tables,
//! Views; key types for Redis), and tables and views that expand to show their columns.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::db::{DbKind, DbObject, ObjectKind, SharedDriver};
use crate::runtime;

const LOADING: &str = "Loading…";

/// Rows of the tree other than objects (which are stored as plain `DbObject`s).
#[derive(Clone)]
enum Node {
    Section { key: String, title: String },
    Column { name: String, data_type: String, primary: bool },
    /// "Loading…", or why the columns can't be shown.
    Status(String),
}

enum Row {
    Section { key: String, title: String },
    Object(DbObject),
    Column { name: String, data_type: String, primary: bool },
    Status(String),
}

/// Pinned objects by (schema, name).
type Pins = Rc<RefCell<HashSet<(String, String)>>>;

type ContextHandler = Rc<RefCell<Option<Box<dyn Fn(DbObject, gtk::Widget, f64, f64)>>>>;

fn boxed(node: Node) -> glib::BoxedAnyObject {
    glib::BoxedAnyObject::new(node)
}

fn row_of(item: &glib::Object) -> Option<Row> {
    let boxed = item.downcast_ref::<glib::BoxedAnyObject>()?;
    if let Ok(obj) = boxed.try_borrow::<DbObject>() {
        return Some(Row::Object(obj.clone()));
    }
    Some(match boxed.try_borrow::<Node>().ok()?.clone() {
        Node::Section { key, title } => Row::Section { key, title },
        Node::Column { name, data_type, primary } => Row::Column { name, data_type, primary },
        Node::Status(text) => Row::Status(text),
    })
}

/// The node behind a row of the flattened tree.
fn tree_row(item: Option<glib::Object>) -> Option<(gtk::TreeListRow, Row)> {
    let row = item?.downcast::<gtk::TreeListRow>().ok()?;
    let node = row_of(&row.item()?)?;
    Some((row, node))
}

fn with_object<R>(item: &glib::Object, f: impl FnOnce(&DbObject) -> R) -> Option<R> {
    let boxed = item.downcast_ref::<glib::BoxedAnyObject>()?;
    let obj = boxed.try_borrow::<DbObject>().ok()?;
    Some(f(&obj))
}

/// Which section an object is listed under.
fn section_of(kind: DbKind, obj: &DbObject) -> String {
    match (kind, obj.kind) {
        (DbKind::Redis, _) => obj.schema.clone(),
        (_, k) if k.is_routine() => "functions".into(),
        (_, ObjectKind::View) => "views".into(),
        _ => "tables".into(),
    }
}

/// Functions start closed (there can be many, and tables are what people look for first).
fn open_by_default(key: &str) -> bool {
    key != "functions"
}

fn section_title(key: &str) -> String {
    match key {
        "functions" => "Functions".into(),
        "tables" => "Tables".into(),
        "views" => "Views".into(),
        // Redis key types.
        other => {
            let mut chars = other.chars();
            chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
        }
    }
}

/// PostgreSQL's long type names, shortened the way psql's `\d` users know them, and
/// without the quotes around mixed-case names like `"deviceKindEnum"`.
fn short_type(data_type: &str) -> String {
    let data_type = data_type.replace('"', "");
    let data_type = data_type.as_str();
    const NAMES: [(&str, &str); 12] = [
        ("character varying", "varchar"),
        ("timestamp with time zone", "timestamptz"),
        ("timestamp without time zone", "timestamp"),
        ("time with time zone", "timetz"),
        ("time without time zone", "time"),
        ("double precision", "float8"),
        ("bigint", "int8"),
        ("integer", "int4"),
        ("smallint", "int2"),
        ("boolean", "bool"),
        ("real", "float4"),
        ("character", "char"),
    ];
    for (long, short) in NAMES {
        if let Some(rest) = data_type.strip_prefix(long)
            && (rest.is_empty() || rest.starts_with(['(', '[']))
        {
            return format!("{short}{rest}");
        }
    }
    data_type.to_string()
}

pub struct Sidebar {
    pub list: gtk::ListView,
    kind: DbKind,
    driver: Rc<RefCell<SharedDriver>>,
    /// Every object, as `DbObject`s.
    store: gio::ListStore,
    /// Pinned objects, listed first in their section with a pin icon.
    pinned: Pins,
    /// `store` with pinned objects first; what the sections list.
    sorted: gtk::SortListModel,
    on_pins_changed: RefCell<Option<Box<dyn Fn(&HashSet<(String, String)>)>>>,
    /// Search text and schema, decided by the workspace.
    filter: gtk::CustomFilter,
    sections: gio::ListStore,
    /// Hides sections with nothing (left) to show.
    section_filter: gtk::CustomFilter,
    children: Rc<RefCell<HashMap<String, gtk::FilterListModel>>>,
    tree: gtk::TreeListModel,
    selection: gtk::SingleSelection,
    /// Column rows per table, filled when the table is first expanded.
    columns: Rc<RefCell<HashMap<(String, String), gio::ListStore>>>,
    loading: RefCell<HashSet<(String, String)>>,
    /// Sections the user opened or closed (by key), kept across reloads and searches and
    /// saved per connection. Others use [`open_by_default`].
    open_sections: RefCell<HashMap<String, bool>>,
    on_sections_changed: RefCell<Option<Box<dyn Fn(&HashMap<String, bool>)>>>,
    /// Set while sections are being refiltered or replaced: rows dropped then collapse,
    /// which is not the user closing them.
    rebuilding: Rc<Cell<bool>>,
    /// Set by a mouse press so the resulting selection change opens the object.
    clicked: Cell<bool>,
    /// Several schemas are listed at once, so names are shown qualified.
    qualify: Rc<Cell<bool>>,
    /// Functions sharing a name (overloads), shown with their argument types.
    overloaded: Rc<RefCell<HashSet<(String, String)>>>,
    refilter_queued: Cell<bool>,
    /// The object to highlight (the open table). Re-applied when rows come back after a
    /// reload, since sections are reopened a moment after the list is replaced.
    wanted: RefCell<Option<DbObject>>,
    reselect_queued: Cell<bool>,
    on_open: RefCell<Option<Box<dyn Fn(DbObject)>>>,
    /// Right-click on an object: the object, the row widget and the click position in it.
    on_context: ContextHandler,
    on_empty: RefCell<Option<Box<dyn Fn(bool)>>>,
}

impl Sidebar {
    pub fn new(kind: DbKind, driver: Rc<RefCell<SharedDriver>>) -> Rc<Self> {
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        // Pinned objects first; otherwise the listing order (the sort is stable).
        let pinned: Pins = Rc::default();
        let pin_sorter = gtk::CustomSorter::new({
            let pinned = pinned.clone();
            move |a, b| {
                let is_pinned = |item: &glib::Object| {
                    with_object(item, |o| pinned.borrow().contains(&(o.schema.clone(), o.name.clone()))).unwrap_or(false)
                };
                is_pinned(b).cmp(&is_pinned(a)).into()
            }
        });
        let sorted = gtk::SortListModel::new(Some(store.clone()), Some(pin_sorter));
        let filter = gtk::CustomFilter::new(|_| true);
        let sections = gio::ListStore::new::<glib::BoxedAnyObject>();
        let children: Rc<RefCell<HashMap<String, gtk::FilterListModel>>> = Rc::default();
        let section_filter = gtk::CustomFilter::new({
            let children = children.clone();
            move |item| match row_of(item) {
                Some(Row::Section { key, .. }) => children.borrow().get(&key).is_some_and(|m| m.n_items() > 0),
                _ => true,
            }
        });
        let root = gtk::FilterListModel::new(Some(sections.clone()), Some(section_filter.clone()));

        let columns: Rc<RefCell<HashMap<(String, String), gio::ListStore>>> = Rc::default();
        // Called to learn whether a row can expand as well as to expand it, so it must
        // not load anything; columns load once the placeholder row is actually shown.
        let tree = gtk::TreeListModel::new(root, false, false, {
            let (children, columns) = (children.clone(), columns.clone());
            move |item| match row_of(item)? {
                Row::Section { key, .. } => children.borrow().get(&key).map(|m| m.clone().upcast()),
                Row::Object(obj) if kind.is_sql() && !obj.kind.is_routine() => {
                    let store = columns
                        .borrow_mut()
                        .entry((obj.schema, obj.name))
                        .or_insert_with(|| {
                            let store = gio::ListStore::new::<glib::BoxedAnyObject>();
                            store.append(&boxed(Node::Status(LOADING.into())));
                            store
                        })
                        .clone();
                    Some(store.upcast())
                }
                _ => None,
            }
        });
        let selection = gtk::SingleSelection::builder().model(&tree).autoselect(false).can_unselect(true).build();

        let qualify = Rc::new(Cell::new(false));
        let overloaded: Rc<RefCell<HashSet<(String, String)>>> = Rc::default();
        let on_context: ContextHandler = Rc::new(RefCell::new(None));
        let factory =
            Self::factory(children.clone(), qualify.clone(), overloaded.clone(), on_context.clone(), pinned.clone());
        let list = gtk::ListView::builder()
            .model(&selection)
            .factory(&factory)
            // Not single-click-activate: in that mode hovering selects rows, which left a
            // stale highlight. A click opens the table; the highlight marks the open table.
            .single_click_activate(false)
            .css_classes(["navigation-sidebar", "table-list"])
            .build();

        let sidebar = Rc::new(Sidebar {
            list,
            kind,
            driver,
            store,
            sorted,
            pinned,
            on_pins_changed: RefCell::new(None),
            filter,
            sections,
            section_filter,
            children,
            tree,
            selection,
            columns,
            loading: RefCell::default(),
            open_sections: RefCell::default(),
            on_sections_changed: RefCell::new(None),
            rebuilding: Rc::default(),
            clicked: Cell::new(false),
            qualify,
            overloaded,
            refilter_queued: Cell::new(false),
            wanted: RefCell::new(None),
            reselect_queued: Cell::new(false),
            on_open: RefCell::new(None),
            on_context,
            on_empty: RefCell::new(None),
        });
        sidebar.connect_signals();
        sidebar
    }

    fn factory(
        children: Rc<RefCell<HashMap<String, gtk::FilterListModel>>>,
        qualify: Rc<Cell<bool>>,
        overloaded: Rc<RefCell<HashSet<(String, String)>>>,
        on_context: ContextHandler,
        pinned: Pins,
    ) -> gtk::SignalListItemFactory {
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let icon = gtk::Image::builder().pixel_size(16).build();
            let name = gtk::Label::builder().xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build();
            let detail = gtk::Label::builder()
                .xalign(1.0)
                .max_width_chars(16)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes(["dim-label", "caption"])
                .build();
            let content = gtk::Box::builder().spacing(8).build();
            let pin = gtk::Image::builder()
                .icon_name("view-pin-symbolic")
                .pixel_size(12)
                .visible(false)
                .tooltip_text("Pinned")
                .css_classes(["pin-icon"])
                .build();
            content.append(&icon);
            content.append(&name);
            content.append(&pin);
            content.append(&detail);
            let expander = gtk::TreeExpander::builder().child(&content).indent_for_icon(true).build();
            // A click anywhere on a section heading opens or closes it.
            let click = gtk::GestureClick::new();
            let e = expander.downgrade();
            click.connect_released(move |_, _, _, _| {
                let Some(row) = e.upgrade().and_then(|e| e.list_row()) else { return };
                if matches!(row.item().as_ref().and_then(row_of), Some(Row::Section { .. })) {
                    row.set_expanded(!row.is_expanded());
                }
            });
            content.add_controller(click);
            // Right-click on an object: its context menu (built by the workspace).
            let menu_click = gtk::GestureClick::builder().button(gtk::gdk::BUTTON_SECONDARY).build();
            let (e, handler) = (expander.downgrade(), on_context.clone());
            menu_click.connect_pressed(move |gesture, _, x, y| {
                let Some(expander) = e.upgrade() else { return };
                let Some(Row::Object(obj)) = expander.list_row().and_then(|r| r.item()).as_ref().and_then(row_of) else { return };
                gesture.set_state(gtk::EventSequenceState::Claimed);
                if let Some(f) = handler.borrow().as_ref() {
                    f(obj, expander.upcast(), x, y);
                }
            });
            expander.add_controller(menu_click);
            item.set_child(Some(&expander));
        });
        factory.connect_bind(move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let Some(expander) = item.child().and_downcast::<gtk::TreeExpander>() else { return };
            let Some((row, node)) = tree_row(item.item()) else { return };
            expander.set_list_row(Some(&row));
            let content = expander.child().unwrap();
            let icon = content.first_child().and_downcast::<gtk::Image>().unwrap();
            let name = icon.next_sibling().and_downcast::<gtk::Label>().unwrap();
            let pin = name.next_sibling().and_downcast::<gtk::Image>().unwrap();
            let detail = content.last_child().and_downcast::<gtk::Label>().unwrap();
            pin.set_visible(false);
            icon.set_visible(true);
            icon.set_opacity(1.0);
            icon.set_css_classes(&[]);
            name.set_css_classes(&[]);
            detail.set_text("");
            content.set_tooltip_text(None);
            item.set_selectable(false);
            match node {
                Row::Section { key, title } => {
                    icon.set_visible(false);
                    name.set_text(&title);
                    name.add_css_class("sidebar-section");
                    // A live count of what the section shows (it follows the search).
                    if let Some(model) = children.borrow().get(&key) {
                        let binding = model
                            .bind_property("n-items", &detail, "label")
                            .transform_to(|_, n: u32| Some(n.to_string()))
                            .sync_create()
                            .build();
                        // SAFETY: only ever stored and taken back as a `glib::Binding`.
                        unsafe { detail.set_data("count", binding) };
                    }
                }
                Row::Object(obj) => {
                    item.set_selectable(true);
                    pin.set_visible(pinned.borrow().contains(&(obj.schema.clone(), obj.name.clone())));
                    icon.set_icon_name(Some(obj.kind.icon_name()));
                    if obj.kind.is_routine() {
                        icon.add_css_class("routine-icon");
                        if overloaded.borrow().contains(&(obj.schema.clone(), obj.name.clone())) {
                            let args: Vec<String> = obj.signature.split(", ").map(short_type).collect();
                            detail.set_text(&format!("({})", args.join(", ")));
                        }
                    }
                    name.set_text(&if qualify.get() { format!("{}.{}", obj.schema, obj.name) } else { obj.name.clone() });
                    let tooltip = match obj.kind {
                        k if k.is_routine() => format!("{}.{}({})", obj.schema, obj.name, obj.signature),
                        ObjectKind::Key => format!("{} ({})", obj.name, obj.schema),
                        _ => format!("{}.{}", obj.schema, obj.name),
                    };
                    content.set_tooltip_text(Some(&tooltip));
                }
                Row::Column { name: column, data_type, primary } => {
                    // The key icon marks primary key columns; others keep the space so names line up.
                    icon.set_icon_name(Some("pueo-key-symbolic"));
                    icon.add_css_class("pk-icon");
                    icon.set_opacity(if primary { 1.0 } else { 0.0 });
                    name.set_text(&column);
                    detail.set_text(&data_type);
                    content.set_tooltip_text(Some(&format!("{column}  {data_type}")));
                }
                Row::Status(text) => {
                    icon.set_visible(false);
                    name.set_text(&text);
                    name.add_css_class("dim-label");
                }
            }
        });
        factory.connect_unbind(|_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let Some(detail) = item.child().and_then(|e| e.last_child()).and_then(|c| c.last_child()) else { return };
            // SAFETY: stored as a `glib::Binding` in bind.
            if let Some(binding) = unsafe { detail.steal_data::<glib::Binding>("count") } {
                binding.unbind();
            }
        });
        factory
    }

    fn connect_signals(self: &Rc<Self>) {
        // New rows: open sections the user hasn't closed, and load columns when a table opens.
        let weak = Rc::downgrade(self);
        self.tree.connect_items_changed(move |tree, pos, _, added| {
            let Some(sidebar) = weak.upgrade() else { return };
            sidebar.queue_reselect();
            let mut sections = Vec::new();
            let mut loads = Vec::new();
            for i in pos..pos + added {
                match tree_row(tree.item(i)) {
                    Some((row, Row::Section { key, .. })) => sections.push((row, key)),
                    Some((row, Row::Status(text))) if text == LOADING => {
                        if let Some((_, Row::Object(obj))) = tree_row(row.parent().map(|p| p.upcast())) {
                            loads.push(obj);
                        }
                    }
                    _ => {}
                }
            }
            if sections.is_empty() && loads.is_empty() {
                return;
            }
            // Not while the model is still announcing this change.
            let weak = Rc::downgrade(&sidebar);
            glib::idle_add_local_once(move || {
                let Some(sidebar) = weak.upgrade() else { return };
                for (row, key) in sections {
                    row.set_expanded(sidebar.is_open(&key));
                    let weak = Rc::downgrade(&sidebar);
                    row.connect_expanded_notify(move |row| {
                        if let Some(sidebar) = weak.upgrade() {
                            sidebar.section_toggled(&key, row.is_expanded());
                        }
                    });
                }
                for obj in loads {
                    sidebar.load_columns(obj);
                }
            });
        });

        // Enter or double-click: open objects, open or close sections and tables.
        let weak = Rc::downgrade(self);
        self.list.connect_activate(move |_, pos| {
            let Some(sidebar) = weak.upgrade() else { return };
            match tree_row(sidebar.tree.item(pos)) {
                Some((_, Row::Object(obj))) => sidebar.open(obj),
                Some((row, Row::Section { .. })) => row.set_expanded(!row.is_expanded()),
                _ => {}
            }
        });
        // A single click selects the row; open it when the selection came from a click.
        let click = gtk::GestureClick::builder().button(gtk::gdk::BUTTON_PRIMARY).build();
        click.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        click.connect_pressed(move |_, _, _, _| {
            if let Some(sidebar) = weak.upgrade() {
                sidebar.clicked.set(true);
            }
        });
        self.list.add_controller(click);
        let weak = Rc::downgrade(self);
        self.selection.connect_selection_changed(move |sel, _, _| {
            let Some(sidebar) = weak.upgrade() else { return };
            if !sidebar.clicked.replace(false) {
                return;
            }
            if let Some((_, Row::Object(obj))) = tree_row(sel.selected_item()) {
                sidebar.open(obj);
            }
        });
    }

    fn is_open(&self, key: &str) -> bool {
        self.open_sections.borrow().get(key).copied().unwrap_or_else(|| open_by_default(key))
    }

    fn section_toggled(&self, key: &str, open: bool) {
        // Rows dropped by a refilter collapse too; that's not the user closing them.
        if self.rebuilding.get() || self.is_open(key) == open {
            return;
        }
        self.open_sections.borrow_mut().insert(key.to_string(), open);
        if let Some(f) = self.on_sections_changed.borrow().as_ref() {
            f(&self.open_sections.borrow());
        }
    }

    /// Restore which sections were open (from the last time this connection was used).
    pub fn set_open_sections(&self, sections: HashMap<String, bool>) {
        *self.open_sections.borrow_mut() = sections;
    }

    /// Called whenever the user opens or closes a section, to save the state.
    pub fn connect_sections_changed(&self, f: impl Fn(&HashMap<String, bool>) + 'static) {
        *self.on_sections_changed.borrow_mut() = Some(Box::new(f));
    }

    fn open(&self, obj: DbObject) {
        if let Some(f) = self.on_open.borrow().as_ref() {
            f(obj);
        }
    }

    pub fn is_pinned(&self, obj: &DbObject) -> bool {
        self.pinned.borrow().contains(&(obj.schema.clone(), obj.name.clone()))
    }

    /// Replace the pins (e.g. the saved ones for this database) without notifying.
    pub fn set_pinned(&self, pins: HashSet<(String, String)>) {
        *self.pinned.borrow_mut() = pins;
        self.store.items_changed(0, self.store.n_items(), self.store.n_items());
    }

    /// Pin or unpin `obj`; it moves to (or from) the top of its section.
    pub fn toggle_pin(&self, obj: &DbObject) {
        let key = (obj.schema.clone(), obj.name.clone());
        {
            let mut pins = self.pinned.borrow_mut();
            if !pins.remove(&key) {
                pins.insert(key);
            }
        }
        // Re-sort and re-draw just that row.
        if let Some(pos) = (0..self.store.n_items()).find(|&i| {
            self.store.item(i).and_then(|o| with_object(&o, |o| o.schema == obj.schema && o.name == obj.name)).unwrap_or(false)
        }) {
            self.store.items_changed(pos, 1, 1);
        }
        if let Some(f) = self.on_pins_changed.borrow().as_ref() {
            f(&self.pinned.borrow());
        }
    }

    /// Called whenever the user pins or unpins something, to save the pins.
    pub fn connect_pins_changed(&self, f: impl Fn(&HashSet<(String, String)>) + 'static) {
        *self.on_pins_changed.borrow_mut() = Some(Box::new(f));
    }

    /// Called on right-click with the object, the row widget and the click position in it.
    pub fn connect_context_menu(&self, f: impl Fn(DbObject, gtk::Widget, f64, f64) + 'static) {
        *self.on_context.borrow_mut() = Some(Box::new(f));
    }

    /// Called when an object is clicked, double-clicked or picked with Enter.
    pub fn connect_open(&self, f: impl Fn(DbObject) + 'static) {
        *self.on_open.borrow_mut() = Some(Box::new(f));
    }

    /// Called with `true` when nothing is visible (no objects, or no search matches).
    pub fn connect_empty(&self, f: impl Fn(bool) + 'static) {
        *self.on_empty.borrow_mut() = Some(Box::new(f));
    }

    /// Which objects are shown (search text, schema); re-run with [`Sidebar::refilter`].
    pub fn set_filter(&self, f: impl Fn(&DbObject) -> bool + 'static) {
        self.filter.set_filter_func(move |item| with_object(item, &f).unwrap_or(true));
    }

    pub fn refilter(&self) {
        self.filter.changed(gtk::FilterChange::Different);
    }

    /// Replace everything listed. Loaded columns are dropped too.
    pub fn set_objects(self: &Rc<Self>, objects: Vec<DbObject>) {
        self.columns.borrow_mut().clear();
        self.loading.borrow_mut().clear();
        let mut schemas = HashSet::new();
        for o in &objects {
            schemas.insert(o.schema.as_str());
        }
        self.qualify.set(matches!(self.kind, DbKind::Mysql) && schemas.len() > 1);
        let mut seen = HashSet::new();
        let mut overloaded = HashSet::new();
        for o in objects.iter().filter(|o| o.kind.is_routine()) {
            let key = (o.schema.clone(), o.name.clone());
            if !seen.insert(key.clone()) {
                overloaded.insert(key);
            }
        }
        *self.overloaded.borrow_mut() = overloaded;

        let keys: Vec<String> = if self.kind.is_sql() {
            ["functions", "tables", "views"].map(String::from).to_vec()
        } else {
            let mut types: Vec<String> = objects.iter().map(|o| o.schema.clone()).collect();
            types.sort();
            types.dedup();
            types
        };
        let mut children = HashMap::new();
        for key in &keys {
            let (k, kind) = (key.clone(), self.kind);
            let in_section = gtk::CustomFilter::new(move |item| with_object(item, |o| section_of(kind, o) == k).unwrap_or(false));
            let every = gtk::EveryFilter::new();
            every.append(self.filter.clone());
            every.append(in_section);
            let model = gtk::FilterListModel::new(Some(self.sorted.clone()), Some(every));
            let weak = Rc::downgrade(self);
            model.connect_items_changed(move |_, _, _, _| {
                if let Some(sidebar) = weak.upgrade() {
                    sidebar.queue_section_refilter();
                }
            });
            children.insert(key.clone(), model);
        }
        *self.children.borrow_mut() = children;
        let items: Vec<glib::BoxedAnyObject> = objects.into_iter().map(glib::BoxedAnyObject::new).collect();
        self.store.splice(0, self.store.n_items(), &items);
        let sections: Vec<glib::BoxedAnyObject> =
            keys.iter().map(|k| boxed(Node::Section { key: k.clone(), title: section_title(k) })).collect();
        self.rebuilding.set(true);
        self.sections.splice(0, self.sections.n_items(), &sections);
        self.rebuilding.set(false);
        self.queue_section_refilter();
    }

    /// Sections appear and disappear as their contents change; batched per main-loop turn.
    fn queue_section_refilter(self: &Rc<Self>) {
        if self.refilter_queued.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            let Some(sidebar) = weak.upgrade() else { return };
            sidebar.refilter_queued.set(false);
            sidebar.rebuilding.set(true);
            sidebar.section_filter.changed(gtk::FilterChange::Different);
            sidebar.rebuilding.set(false);
            let empty = sidebar.tree.n_items() == 0;
            if let Some(f) = sidebar.on_empty.borrow().as_ref() {
                f(empty);
            }
        });
    }

    fn load_columns(self: &Rc<Self>, obj: DbObject) {
        let key = (obj.schema.clone(), obj.name.clone());
        if !self.loading.borrow_mut().insert(key.clone()) {
            return;
        }
        let driver = self.driver.borrow().clone();
        let weak = Rc::downgrade(self);
        runtime::spawn(async move { driver.structure(&obj).await }, move |res| {
            let Some(sidebar) = weak.upgrade() else { return };
            // Gone if the list was reloaded meanwhile.
            let Some(store) = sidebar.columns.borrow().get(&key).cloned() else { return };
            let nodes: Vec<Node> = match res {
                Ok(r) => {
                    let idx = |name: &str| r.columns.iter().position(|c| c == name);
                    let (Some(c), t, k) = (idx("column"), idx("type"), idx("key")) else {
                        return store.splice(0, store.n_items(), &[boxed(Node::Status("No columns".into()))]);
                    };
                    let cell = |row: &Vec<Option<String>>, i: Option<usize>| i.and_then(|i| row[i].clone()).unwrap_or_default();
                    r.rows
                        .iter()
                        .map(|row| Node::Column {
                            name: cell(row, Some(c)),
                            data_type: short_type(&cell(row, t)),
                            primary: cell(row, k) == "PRI",
                        })
                        .collect()
                }
                Err(e) => vec![Node::Status(format!("Could not load columns: {e:#}"))],
            };
            let nodes = if nodes.is_empty() { vec![Node::Status("No columns".into())] } else { nodes };
            let items: Vec<glib::BoxedAnyObject> = nodes.into_iter().map(boxed).collect();
            store.splice(0, store.n_items(), &items);
        });
    }

    /// Highlight `obj` (the open table), or nothing.
    pub fn select(&self, obj: Option<&DbObject>) {
        *self.wanted.borrow_mut() = obj.cloned();
        self.apply_selection();
    }

    /// Rows were added or removed: bring back the highlight if it was lost.
    fn queue_reselect(self: &Rc<Self>) {
        if self.wanted.borrow().is_none() || self.reselect_queued.replace(true) {
            return;
        }
        // Batched per main-loop turn; reopening sections adds rows again, which queues another try.
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            let Some(sidebar) = weak.upgrade() else { return };
            sidebar.reselect_queued.set(false);
            let selected = tree_row(sidebar.selection.selected_item()).and_then(|(_, row)| match row {
                Row::Object(obj) => Some(obj),
                _ => None,
            });
            if selected != *sidebar.wanted.borrow() {
                sidebar.apply_selection();
            }
        });
    }

    fn apply_selection(&self) {
        self.clicked.set(false);
        let wanted = self.wanted.borrow().clone();
        let pos = wanted.as_ref().and_then(|obj| {
            (0..self.tree.n_items()).find(|&i| {
                self.tree
                    .item(i)
                    .and_downcast::<gtk::TreeListRow>()
                    .and_then(|row| row.item())
                    .and_then(|item| with_object(&item, |o| o == obj))
                    .unwrap_or(false)
            })
        });
        self.selection.set_selected(pos.unwrap_or(gtk::INVALID_LIST_POSITION));
    }

    /// The first object shown, for Enter in the search box.
    pub fn first_object(&self) -> Option<DbObject> {
        (0..self.tree.n_items()).find_map(|i| match tree_row(self.tree.item(i)) {
            Some((_, Row::Object(obj))) => Some(obj),
            _ => None,
        })
    }

    /// Every listed object (ignoring the filter).
    pub fn objects(&self) -> Vec<DbObject> {
        (0..self.store.n_items()).filter_map(|i| self.store.item(i).and_then(|o| with_object(&o, DbObject::clone))).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortens_postgres_types() {
        assert_eq!(short_type("character varying(255)"), "varchar(255)");
        assert_eq!(short_type("timestamp with time zone"), "timestamptz");
        assert_eq!(short_type("integer"), "int4");
        assert_eq!(short_type("integer[]"), "int4[]");
        assert_eq!(short_type("interval"), "interval");
        assert_eq!(short_type("realm"), "realm");
        assert_eq!(short_type("\"deviceKindEnum\""), "deviceKindEnum");
    }
}
