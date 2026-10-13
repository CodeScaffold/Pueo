//! Right-hand panel showing the selected row as an editable form.

use std::cell::{Cell as StdCell, RefCell};
use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk::glib;

use super::data_grid::DataGrid;
use crate::db::{Cell, ColumnChoices};

/// Values longer than this (or with line breaks, or JSON) get a multi-line editor.
const LONG_VALUE: usize = 40;

enum Editor {
    Entry(gtk::Entry),
    /// Multi-line text inside its own scrolled box, plus the text as first shown.
    Text(gtk::TextView, gtk::ScrolledWindow, String),
    Choice(gtk::DropDown, Vec<Cell>),
}

impl Editor {
    fn widget(&self) -> gtk::Widget {
        match self {
            Editor::Entry(e) => e.clone().upcast(),
            Editor::Text(_, scroll, _) => scroll.clone().upcast(),
            Editor::Choice(d, _) => d.clone().upcast(),
        }
    }

    fn has_focus(&self) -> bool {
        match self {
            Editor::Entry(e) => e.has_focus() || e.focus_child().is_some(),
            Editor::Text(t, _, _) => t.has_focus(),
            Editor::Choice(..) => false,
        }
    }
}

pub struct RowInspector {
    pub widget: gtk::Box,
    grid: Weak<DataGrid>,
    title: gtk::Label,
    stack: gtk::Stack,
    placeholder: adw::StatusPage,
    fields_box: gtk::Box,
    editors: RefCell<Vec<Editor>>,
    /// Row being shown, as a position in the grid.
    pos: StdCell<Option<u32>>,
    /// Suppresses change handlers while fields are being filled in.
    filling: StdCell<bool>,
}

/// Indent JSON objects and arrays for reading; anything else is returned unchanged.
fn pretty_json(value: &str) -> String {
    let trimmed = value.trim_start();
    if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
        return value.to_string();
    }
    serde_json::from_str::<serde_json::Value>(value)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| value.to_string())
}

fn is_long(value: &str) -> bool {
    let v = value.trim_start();
    value.contains('\n') || value.chars().count() > LONG_VALUE || v.starts_with('{') || v.starts_with('[')
}

impl RowInspector {
    pub fn new(grid: &Rc<DataGrid>) -> Rc<Self> {
        let title = gtk::Label::builder().xalign(0.0).hexpand(true).css_classes(["heading"]).build();
        let header = gtk::Box::builder().margin_start(12).margin_end(12).margin_top(10).margin_bottom(6).build();
        header.append(&title);

        let placeholder = adw::StatusPage::builder()
            .icon_name("view-list-symbolic")
            .title("No Row Selected")
            .description("Select a row to see and edit all of its columns here")
            .css_classes(["compact"])
            .build();
        let fields_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .margin_start(12)
            .margin_end(12)
            .margin_top(4)
            .margin_bottom(16)
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .child(&fields_box)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        let stack = gtk::Stack::new();
        stack.add_named(&placeholder, Some("empty"));
        stack.add_named(&scroll, Some("fields"));

        let widget = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .width_request(280)
            .css_classes(["row-inspector"])
            .build();
        widget.append(&header);
        widget.append(&stack);

        let inspector = Rc::new(RowInspector {
            widget,
            grid: Rc::downgrade(grid),
            title,
            stack,
            placeholder,
            fields_box,
            editors: RefCell::new(Vec::new()),
            pos: StdCell::new(None),
            filling: StdCell::new(false),
        });
        inspector.show_selection();
        inspector
    }

    /// Rebuild for the grid's current selection.
    pub fn show_selection(self: &Rc<Self>) {
        let Some(grid) = self.grid.upgrade() else { return };
        let selected = grid.selected_positions();
        match selected.as_slice() {
            [pos] => self.show_row(*pos),
            [] => self.show_message("Row", "No Row Selected", "Select a row to see and edit all of its columns here"),
            many => self.show_message(
                "Rows",
                &format!("{} Rows Selected", many.len()),
                "Select a single row to edit it here",
            ),
        }
    }

    /// Refresh values after the row changed elsewhere (e.g. edited in the grid),
    /// unless the user is typing in one of the fields.
    pub fn refresh(self: &Rc<Self>) {
        if self.editors.borrow().iter().any(Editor::has_focus) {
            return;
        }
        self.show_selection();
    }

    fn show_message(&self, title: &str, heading: &str, description: &str) {
        self.pos.set(None);
        self.title.set_text(title);
        self.placeholder.set_title(heading);
        self.placeholder.set_description(Some(description));
        self.stack.set_visible_child_name("empty");
    }

    fn show_row(self: &Rc<Self>, pos: u32) {
        let Some(grid) = self.grid.upgrade() else { return };
        let Some(row) = grid.row_at(pos) else { return };
        self.pos.set(Some(pos));
        let editable = grid.is_editable() && !row.deleted;
        let state = if row.deleted {
            " (deleted)"
        } else if row.original.is_none() {
            " (new)"
        } else {
            ""
        };
        self.title.set_text(&format!("Row {}{state}", pos + 1));

        self.filling.set(true);
        while let Some(child) = self.fields_box.first_child() {
            self.fields_box.remove(&child);
        }
        let mut editors = Vec::new();
        for (idx, column) in grid.columns().iter().enumerate() {
            let value = row.current.get(idx).cloned().flatten();
            let modified = row.original.as_ref().is_some_and(|o| o.get(idx) != row.current.get(idx));
            let editor = self.field(idx, column, value, grid.choices_for(column), editable, modified);
            editors.push(editor);
        }
        *self.editors.borrow_mut() = editors;
        self.filling.set(false);
        self.stack.set_visible_child_name("fields");
    }

    /// One labelled field; returns its editor.
    fn field(
        self: &Rc<Self>,
        idx: usize,
        column: &str,
        value: Cell,
        choices: Option<ColumnChoices>,
        editable: bool,
        modified: bool,
    ) -> Editor {
        let label = gtk::Label::builder()
            .label(column)
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["caption-heading", "dim-label"])
            .build();
        let null_button = gtk::Button::builder()
            .label("NULL")
            .tooltip_text("Set NULL")
            .sensitive(editable && value.is_some())
            // Not a focus stop: after the form redraws, focus would land on it.
            .can_focus(false)
            .css_classes(["flat", "inspector-null"])
            .build();
        let label_row = gtk::Box::builder().spacing(6).build();
        label_row.append(&label);
        if editable {
            label_row.append(&null_button);
        }

        let editor = match choices {
            Some(choices) => {
                let mut options: Vec<Cell> = choices.values.iter().cloned().map(Some).collect();
                if choices.nullable {
                    options.push(None);
                }
                let names: Vec<&str> = options.iter().map(|o| o.as_deref().unwrap_or("NULL")).collect();
                let dropdown = gtk::DropDown::from_strings(&names);
                let current = options
                    .iter()
                    .position(|o| match (o.as_deref(), value.as_deref()) {
                        (Some("true"), Some("t")) | (Some("false"), Some("f")) => true,
                        _ => *o == value,
                    })
                    .map_or(gtk::INVALID_LIST_POSITION, |p| p as u32);
                dropdown.set_selected(current);
                dropdown.set_sensitive(editable);
                Editor::Choice(dropdown, options)
            }
            None if value.as_deref().is_some_and(is_long) => {
                let text = gtk::TextView::builder()
                    .wrap_mode(gtk::WrapMode::WordChar)
                    .monospace(true)
                    .editable(editable)
                    .top_margin(6)
                    .bottom_margin(6)
                    .left_margin(8)
                    .right_margin(8)
                    .css_classes(["inspector-text"])
                    .build();
                let shown = pretty_json(value.as_deref().unwrap_or(""));
                text.buffer().set_text(&shown);
                // A fixed range (about 5 to 14 lines) that scrolls inside, so the height is
                // right from the first frame and long JSON doesn't stretch the whole form.
                let scroll = gtk::ScrolledWindow::builder()
                    .child(&text)
                    .hscrollbar_policy(gtk::PolicyType::Never)
                    .min_content_height(96)
                    .max_content_height(260)
                    .propagate_natural_height(true)
                    .css_classes(["inspector-text-box"])
                    .build();
                Editor::Text(text, scroll, shown)
            }
            None => {
                let entry = gtk::Entry::builder()
                    .text(value.as_deref().unwrap_or(""))
                    .placeholder_text(if value.is_none() { "NULL" } else { "" })
                    .editable(editable)
                    .css_classes(["monospace"])
                    .build();
                Editor::Entry(entry)
            }
        };
        let editor_widget = editor.widget();
        if modified {
            editor_widget.add_css_class("inspector-modified");
        }

        let field = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).build();
        field.append(&label_row);
        field.append(&editor_widget);
        self.fields_box.append(&field);

        // Wire up saving.
        let weak = Rc::downgrade(self);
        null_button.connect_clicked(move |_| {
            if let Some(i) = weak.upgrade() {
                i.commit(idx, None);
            }
        });
        match &editor {
            Editor::Entry(entry) => {
                let original = value.clone();
                let save = {
                    let weak = Rc::downgrade(self);
                    move |e: &gtk::Entry| {
                        let text = e.text().to_string();
                        // An untouched NULL stays NULL instead of becoming ''.
                        if original.as_deref().unwrap_or("") != text
                            && let Some(i) = weak.upgrade()
                        {
                            i.commit(idx, Some(text));
                        }
                    }
                };
                let s = save.clone();
                entry.connect_activate(move |e| s(e));
                let focus = gtk::EventControllerFocus::new();
                let e = entry.downgrade();
                focus.connect_leave(move |_| {
                    if let Some(e) = e.upgrade() {
                        save(&e);
                    }
                });
                entry.add_controller(focus);
            }
            Editor::Text(text, _, shown) => {
                // Compare with what was shown (JSON is pretty-printed), so just looking
                // at a value never rewrites it.
                let shown = shown.clone();
                let focus = gtk::EventControllerFocus::new();
                let (weak, t) = (Rc::downgrade(self), text.downgrade());
                focus.connect_leave(move |_| {
                    let (Some(i), Some(t)) = (weak.upgrade(), t.upgrade()) else { return };
                    let b = t.buffer();
                    let content = b.text(&b.start_iter(), &b.end_iter(), false).to_string();
                    if shown != content {
                        i.commit(idx, Some(content));
                    }
                });
                text.add_controller(focus);
            }
            Editor::Choice(dropdown, options) => {
                let (weak, options) = (Rc::downgrade(self), options.clone());
                dropdown.connect_selected_notify(move |d| {
                    let Some(i) = weak.upgrade() else { return };
                    if i.filling.get() {
                        return;
                    }
                    if let Some(v) = options.get(d.selected() as usize) {
                        i.commit(idx, v.clone());
                    }
                });
            }
        }
        editor
    }

    /// Save a value into the grid as a pending change, then redraw the form.
    fn commit(self: &Rc<Self>, idx: usize, value: Cell) {
        let (Some(grid), Some(pos)) = (self.grid.upgrade(), self.pos.get()) else { return };
        grid.set_cell_at(pos, idx, value);
        // Redraw after the current event so focus moves settle first.
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(i) = weak.upgrade() {
                i.refresh();
            }
        });
    }
}
