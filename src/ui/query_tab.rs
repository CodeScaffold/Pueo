use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use super::completion::{Completion, SharedSchema};
use super::data_grid::DataGrid;
use super::editor;
use crate::db::{DbKind, QueryResult, SharedDriver};
use crate::export::{self, Format};
use crate::library::{self, HistoryEntry};
use crate::runtime;

const HISTORY_LIMIT: usize = 300;

pub struct QueryTab {
    pub widget: gtk::Paned,
    driver: SharedDriver,
    kind: DbKind,
    connection_id: String,
    editor: gtk::TextView,
    run_button: gtk::Button,
    run_all_button: gtk::Button,
    stop_button: gtk::Button,
    spinner: adw::Spinner,
    results: gtk::Stack,
    switcher: gtk::StackSwitcher,
    status: gtk::Label,
    saved_list: gtk::ListBox,
    history_list: gtk::ListBox,
    history_search: gtk::SearchEntry,
    library_popover: gtk::Popover,
    actions: gio::SimpleActionGroup,
    running: Cell<bool>,
    /// Keeps the grids of the current results alive.
    grids: RefCell<Vec<Rc<DataGrid>>>,
    _completion: Rc<Completion>,
}

fn first_line(sql: &str) -> String {
    let line = sql.trim().lines().next().unwrap_or("").trim();
    let mut s: String = line.chars().take(90).collect();
    if line.chars().count() > 90 || sql.trim().lines().count() > 1 {
        s.push('…');
    }
    s
}

impl QueryTab {
    pub fn new(
        driver: SharedDriver,
        kind: DbKind,
        connection_id: String,
        schema: SharedSchema,
    ) -> Rc<Self> {
        let editor = editor::create(kind);
        editor.set_monospace(true);
        editor.set_top_margin(10);
        editor.set_bottom_margin(10);
        editor.set_left_margin(8);
        editor.set_right_margin(12);
        editor.set_wrap_mode(gtk::WrapMode::WordChar);
        editor.add_css_class("query-editor");
        let editor_scroll = gtk::ScrolledWindow::builder().child(&editor).vexpand(true).build();
        let completion = Completion::attach(&editor, kind, schema);

        let run_button = gtk::Button::builder()
            .child(
                &adw::ButtonContent::builder()
                    .icon_name("media-playback-start-symbolic")
                    .label(if kind == DbKind::Redis { "Run Line" } else { "Run" })
                    .build(),
            )
            .tooltip_text("Run the selection or the statement at the cursor (Ctrl+Enter)")
            .css_classes(["suggested-action", "pill-small"])
            .build();
        let run_all_button = gtk::Button::builder()
            .label("Run All")
            .tooltip_text("Run everything in the editor (Ctrl+Shift+Enter)")
            .css_classes(["flat"])
            .build();
        let stop_button = gtk::Button::builder()
            .child(
                &adw::ButtonContent::builder()
                    .icon_name("media-playback-stop-symbolic")
                    .label("Stop")
                    .build(),
            )
            .tooltip_text("Cancel the running query")
            .css_classes(["destructive-action", "pill-small"])
            .visible(false)
            .build();
        let spinner = adw::Spinner::builder().visible(false).build();
        let hint = gtk::Label::builder()
            .label("Ctrl+Space to complete")
            .css_classes(["dim-label", "caption"])
            .hexpand(true)
            .xalign(0.0)
            .build();

        let save_button = gtk::Button::builder()
            .icon_name("bookmark-new-symbolic")
            .tooltip_text("Save Query (Ctrl+S)")
            .action_name("query.save")
            .css_classes(["flat"])
            .build();

        // Saved queries & history popover.
        let saved_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["navigation-sidebar"])
            .build();
        saved_list.set_placeholder(Some(
            &gtk::Label::builder()
                .label("No saved queries yet.\nUse the bookmark button to save one.")
                .justify(gtk::Justification::Center)
                .css_classes(["dim-label"])
                .margin_top(24)
                .margin_bottom(24)
                .build(),
        ));
        let history_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["navigation-sidebar"])
            .build();
        history_list.set_placeholder(Some(
            &gtk::Label::builder()
                .label("No history")
                .css_classes(["dim-label"])
                .margin_top(24)
                .margin_bottom(24)
                .build(),
        ));
        let history_search = gtk::SearchEntry::builder().placeholder_text("Search history").build();
        let clear_history = gtk::Button::builder()
            .label("Clear History")
            .action_name("query.clear-history")
            .css_classes(["flat", "destructive-action"])
            .halign(gtk::Align::End)
            .build();
        let history_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
        history_box.append(&history_search);
        history_box.append(
            &gtk::ScrolledWindow::builder()
                .child(&history_list)
                .vexpand(true)
                .hscrollbar_policy(gtk::PolicyType::Never)
                .build(),
        );
        history_box.append(&clear_history);
        let library_stack = gtk::Stack::builder().vexpand(true).build();
        library_stack.add_titled(
            &gtk::ScrolledWindow::builder()
                .child(&saved_list)
                .hscrollbar_policy(gtk::PolicyType::Never)
                .build(),
            Some("saved"),
            "Saved",
        );
        library_stack.add_titled(&history_box, Some("history"), "History");
        let library_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .width_request(460)
            .height_request(420)
            .build();
        library_box.append(&gtk::StackSwitcher::builder().stack(&library_stack).halign(gtk::Align::Center).build());
        library_box.append(&library_stack);
        let library_popover = gtk::Popover::builder().child(&library_box).build();
        let library_button = gtk::MenuButton::builder()
            .icon_name("document-open-recent-symbolic")
            .tooltip_text("Saved Queries & History")
            .popover(&library_popover)
            .css_classes(["flat"])
            .build();

        let export_menu = gio::Menu::new();
        export_menu.append(Some("Export Result as CSV…"), Some("query.export::csv"));
        export_menu.append(Some("Export Result as JSON…"), Some("query.export::json"));
        export_menu.append(Some("Export Result as SQL…"), Some("query.export::sql"));
        let export_button = gtk::MenuButton::builder()
            .icon_name("document-send-symbolic")
            .tooltip_text("Export Result")
            .menu_model(&export_menu)
            .css_classes(["flat"])
            .build();

        let toolbar = gtk::Box::builder()
            .spacing(6)
            .margin_start(8)
            .margin_end(8)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        toolbar.append(&run_button);
        toolbar.append(&stop_button);
        if kind.is_sql() {
            toolbar.append(&run_all_button);
        }
        toolbar.append(&spinner);
        toolbar.append(&hint);
        toolbar.append(&save_button);
        toolbar.append(&library_button);
        toolbar.append(&export_button);

        let top = gtk::Box::new(gtk::Orientation::Vertical, 0);
        top.append(&toolbar);
        top.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        top.append(&editor_scroll);

        let results = gtk::Stack::builder().vexpand(true).build();
        let placeholder = adw::StatusPage::builder()
            .icon_name("utilities-terminal-symbolic")
            .title("No Results")
            .description("Write a query above and press Ctrl+Enter")
            .css_classes(["compact"])
            .build();
        results.add_named(&placeholder, Some("placeholder"));
        let switcher = gtk::StackSwitcher::builder().stack(&results).visible(false).build();
        let status = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .selectable(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["caption"])
            .build();
        let footer = gtk::Box::builder().spacing(8).css_classes(["statusbar"]).build();
        footer.append(&switcher);
        footer.append(&status);

        let bottom = gtk::Box::new(gtk::Orientation::Vertical, 0);
        bottom.append(&results);
        bottom.append(&footer);

        let widget = gtk::Paned::builder()
            .orientation(gtk::Orientation::Vertical)
            .start_child(&top)
            .end_child(&bottom)
            .position(260)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .wide_handle(true)
            .build();

        let tab = Rc::new(QueryTab {
            widget,
            driver,
            kind,
            connection_id,
            editor,
            run_button,
            run_all_button,
            stop_button,
            spinner,
            results,
            switcher,
            status,
            saved_list,
            history_list,
            history_search,
            library_popover,
            actions: gio::SimpleActionGroup::new(),
            running: Cell::new(false),
            grids: RefCell::new(Vec::new()),
            _completion: completion,
        });
        tab.connect_signals();
        tab.setup_actions();
        tab
    }

    fn connect_signals(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.run_button.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.run_current();
            }
        });
        let weak = Rc::downgrade(self);
        self.run_all_button.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.run_all();
            }
        });
        let weak = Rc::downgrade(self);
        self.stop_button.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.cancel();
            }
        });

        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, state| {
            let is_enter = matches!(key, gdk::Key::Return | gdk::Key::KP_Enter);
            if is_enter && state.contains(gdk::ModifierType::CONTROL_MASK) {
                if let Some(t) = weak.upgrade() {
                    if state.contains(gdk::ModifierType::SHIFT_MASK) {
                        t.run_all();
                    } else {
                        t.run_current();
                    }
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.editor.add_controller(keys);

        let weak = Rc::downgrade(self);
        self.library_popover.connect_show(move |_| {
            if let Some(t) = weak.upgrade() {
                t.refresh_library();
            }
        });
        let weak = Rc::downgrade(self);
        self.history_search.connect_search_changed(move |_| {
            if let Some(t) = weak.upgrade() {
                t.refresh_history();
            }
        });
    }

    fn setup_actions(self: &Rc<Self>) {
        let save = gio::SimpleAction::new("save", None);
        let weak = Rc::downgrade(self);
        save.connect_activate(move |_, _| {
            if let Some(t) = weak.upgrade() {
                t.save_dialog();
            }
        });
        self.actions.add_action(&save);

        let clear = gio::SimpleAction::new("clear-history", None);
        let weak = Rc::downgrade(self);
        clear.connect_activate(move |_, _| {
            let Some(t) = weak.upgrade() else { return };
            if let Err(e) = library::clear_history(&t.connection_id) {
                t.flash(&format!("Could not clear history: {e}"));
            }
            t.refresh_history();
        });
        self.actions.add_action(&clear);

        let export = gio::SimpleAction::new("export", Some(glib::VariantTy::STRING));
        let weak = Rc::downgrade(self);
        export.connect_activate(move |_, param| {
            if let (Some(t), Some(fmt)) = (weak.upgrade(), param.and_then(|p| p.str().and_then(Format::from_name))) {
                t.choose_export(fmt);
            }
        });
        self.actions.add_action(&export);
        self.widget.insert_action_group("query", Some(&self.actions));
    }

    pub fn set_text(&self, text: &str) {
        let buffer = self.editor.buffer();
        buffer.begin_user_action();
        buffer.set_text(text);
        buffer.end_user_action();
    }

    pub fn focus(&self) {
        self.editor.grab_focus();
    }

    fn full_text(&self) -> String {
        let buffer = self.editor.buffer();
        buffer.text(&buffer.start_iter(), &buffer.end_iter(), false).to_string()
    }

    /// The selection, or the statement (SQL) / line (Redis) at the cursor.
    fn current_text(&self) -> String {
        let buffer = self.editor.buffer();
        if let Some((start, end)) = buffer.selection_bounds() {
            return buffer.text(&start, &end, false).to_string();
        }
        let cursor = buffer.iter_at_mark(&buffer.get_insert());
        if self.kind == DbKind::Redis {
            let mut start = cursor;
            start.set_line_offset(0);
            let mut end = cursor;
            if !end.ends_line() {
                end.forward_to_line_end();
            }
            return buffer.text(&start, &end, false).to_string();
        }
        let text = self.full_text();
        match editor::statement_at(&text, cursor.offset() as usize) {
            Some((s, e)) => text.chars().skip(s).take(e - s).collect(),
            None => String::new(),
        }
    }

    pub fn run_current(self: &Rc<Self>) {
        let sql = self.current_text();
        self.run(sql);
    }

    pub fn run_all(self: &Rc<Self>) {
        let sql = self.full_text();
        self.run(sql);
    }

    fn set_running(&self, running: bool) {
        self.running.set(running);
        self.run_button.set_visible(!running);
        self.stop_button.set_visible(running);
        self.run_all_button.set_sensitive(!running);
        self.spinner.set_visible(running);
    }

    fn run(self: &Rc<Self>, sql: String) {
        if sql.trim().is_empty() || self.running.get() {
            return;
        }
        self.set_running(true);
        self.status.remove_css_class("error");
        self.status.set_tooltip_text(None);
        self.status.set_text("Running…");

        let driver = self.driver.clone();
        let weak = Rc::downgrade(self);
        let history_sql = sql.trim().to_string();
        runtime::spawn(
            async move {
                let start = Instant::now();
                let res = driver.execute(&sql).await;
                (res, start.elapsed())
            },
            move |(res, elapsed)| {
                let Some(t) = weak.upgrade() else { return };
                t.set_running(false);
                let entry = HistoryEntry {
                    connection_id: t.connection_id.clone(),
                    sql: history_sql,
                    at: library::now(),
                    ok: res.is_ok(),
                    millis: elapsed.as_millis() as u64,
                };
                if let Err(e) = library::append_history(&entry) {
                    eprintln!("could not write history: {e}");
                }
                match res {
                    Ok(results) => t.show_results(results, elapsed),
                    Err(e) => {
                        t.status.add_css_class("error");
                        t.status.set_text(&format!("{e:#}"));
                        t.status.set_tooltip_text(Some(&format!("{e:#}")));
                    }
                }
            },
        );
    }

    pub fn cancel(self: &Rc<Self>) {
        if !self.running.get() {
            return;
        }
        self.status.set_text("Cancelling…");
        let driver = self.driver.clone();
        let weak = Rc::downgrade(self);
        runtime::spawn(async move { driver.cancel().await }, move |res| {
            if let (Some(t), Err(e)) = (weak.upgrade(), res) {
                t.flash(&format!("Could not cancel: {e:#}"));
            }
        });
    }

    /// A short message in the status line.
    fn flash(&self, msg: &str) {
        self.status.remove_css_class("error");
        self.status.set_tooltip_text(Some(msg));
        self.status.set_text(msg);
    }

    fn show_results(&self, results: Vec<QueryResult>, elapsed: std::time::Duration) {
        // Drop previous result pages.
        while let Some(child) = self.results.first_child() {
            self.results.remove(&child);
        }
        self.grids.borrow_mut().clear();

        let mut summary = Vec::new();
        for (i, r) in results.iter().enumerate() {
            let name = format!("r{i}");
            let title = format!("Result {}", i + 1);
            if r.has_rows() {
                let grid = DataGrid::new(false);
                grid.set_result(r);
                self.results.add_titled(&grid.widget, Some(&name), &title);
                self.grids.borrow_mut().push(grid);
                summary.push(format!("{} rows", r.rows.len()));
            } else {
                let affected = r.affected.unwrap_or(0);
                let page = adw::StatusPage::builder()
                    .icon_name("object-select-symbolic")
                    .title("Query Executed")
                    .description(format!("{affected} row(s) affected"))
                    .css_classes(["compact"])
                    .build();
                self.results.add_titled(&page, Some(&name), &title);
                self.grids.borrow_mut().push(DataGrid::new(false));
                summary.push(format!("{affected} affected"));
            }
        }
        if results.is_empty() {
            let page = adw::StatusPage::builder().title("No statements executed").css_classes(["compact"]).build();
            self.results.add_named(&page, Some("empty"));
        }
        // Show the last result, like most SQL clients.
        if let Some(last) = results.len().checked_sub(1) {
            self.results.set_visible_child_name(&format!("r{last}"));
        }
        self.switcher.set_visible(results.len() > 1);
        let shown = if summary.len() > 3 { format!("{} statements", summary.len()) } else { summary.join(" · ") };
        self.status.set_text(&format!("{shown} · {} ms", elapsed.as_millis()));
    }

    // ---------- saved queries & history ----------

    fn refresh_library(self: &Rc<Self>) {
        self.saved_list.remove_all();
        for q in library::load_saved(&self.connection_id) {
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&q.name))
                .subtitle(glib::markup_escape_text(&first_line(&q.sql)))
                .subtitle_lines(1)
                .activatable(true)
                .build();
            let delete = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text("Delete")
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            row.add_suffix(&delete);
            let weak = Rc::downgrade(self);
            let id = q.id.clone();
            delete.connect_clicked(move |_| {
                let Some(t) = weak.upgrade() else { return };
                if let Err(e) = library::delete_saved(&id) {
                    t.flash(&format!("Could not delete: {e}"));
                }
                t.refresh_library();
            });
            let weak = Rc::downgrade(self);
            row.connect_activated(move |_| {
                if let Some(t) = weak.upgrade() {
                    t.set_text(&q.sql);
                    t.library_popover.popdown();
                    t.focus();
                }
            });
            self.saved_list.append(&row);
        }
        self.refresh_history();
    }

    fn refresh_history(self: &Rc<Self>) {
        self.history_list.remove_all();
        let needle = self.history_search.text().to_lowercase();
        for entry in library::load_history(&self.connection_id, HISTORY_LIMIT) {
            if !needle.is_empty() && !entry.sql.to_lowercase().contains(&needle) {
                continue;
            }
            let when = glib::DateTime::from_unix_local(entry.at)
                .ok()
                .and_then(|d| d.format("%b %e, %H:%M").ok())
                .map(|s| s.to_string())
                .unwrap_or_default();
            let subtitle = format!("{when} · {} ms{}", entry.millis, if entry.ok { "" } else { " · failed" });
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&first_line(&entry.sql)))
                .title_lines(1)
                .subtitle(subtitle)
                .activatable(true)
                .tooltip_text(&entry.sql)
                .build();
            if !entry.ok {
                row.add_prefix(&gtk::Image::from_icon_name("dialog-error-symbolic"));
            }
            let weak = Rc::downgrade(self);
            row.connect_activated(move |_| {
                if let Some(t) = weak.upgrade() {
                    t.set_text(&entry.sql);
                    t.library_popover.popdown();
                    t.focus();
                }
            });
            self.history_list.append(&row);
        }
    }

    pub fn save_dialog(self: &Rc<Self>) {
        let sql = self.full_text();
        if sql.trim().is_empty() {
            self.flash("Nothing to save");
            return;
        }
        let entry = gtk::Entry::builder().placeholder_text("Name").activates_default(true).build();
        let dialog = adw::AlertDialog::new(Some("Save Query"), Some("Saved queries are available from the history button in every query tab of this connection."));
        dialog.set_extra_child(Some(&entry));
        dialog.add_responses(&[("cancel", "Cancel"), ("save", "Save")]);
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("cancel");
        let weak = Rc::downgrade(self);
        dialog.connect_response(None, move |_, response| {
            let Some(t) = weak.upgrade() else { return };
            let name = entry.text().trim().to_string();
            if response != "save" || name.is_empty() {
                return;
            }
            match library::save_query(&t.connection_id, &name, &sql) {
                Ok(()) => t.flash(&format!("Saved \"{name}\"")),
                Err(e) => t.flash(&format!("Could not save: {e}")),
            }
        });
        dialog.present(Some(&self.widget));
    }

    // ---------- export ----------

    fn visible_grid(&self) -> Option<Rc<DataGrid>> {
        let name = self.results.visible_child_name()?;
        let idx: usize = name.strip_prefix('r')?.parse().ok()?;
        self.grids.borrow().get(idx).cloned()
    }

    fn choose_export(self: &Rc<Self>, format: Format) {
        let Some(grid) = self.visible_grid().filter(|g| !g.columns().is_empty()) else {
            self.flash("Run a query that returns rows first");
            return;
        };
        let dialog = gtk::FileDialog::builder()
            .title("Export Result")
            .initial_name(format!("result.{}", format.extension()))
            .build();
        let weak = Rc::downgrade(self);
        let window = self.widget.root().and_downcast::<gtk::Window>();
        dialog.save(window.as_ref(), None::<&gio::Cancellable>, move |res| {
            let (Some(t), Ok(file)) = (weak.upgrade(), res) else { return };
            let Some(path) = file.path() else { return };
            t.export_grid(&grid, path, format);
        });
    }

    fn export_grid(&self, grid: &DataGrid, path: PathBuf, format: Format) {
        let (columns, rows) = (grid.columns(), grid.rows());
        match export::write(&path, format, self.kind, "query_result", &columns, &rows) {
            Ok(()) => self.flash(&format!("Exported {} rows to {}", rows.len(), path.display())),
            Err(e) => self.flash(&format!("Export failed: {e:#}")),
        }
    }
}
