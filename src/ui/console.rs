//! Bottom panel listing every query sent to the server.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use adw::prelude::*;
use gtk::glib;

use super::highlight;
use crate::db::DbKind;
use crate::db::log::{LogEntry, LogKind, QueryLog};

/// How often new entries are picked up while the panel is open.
const POLL: Duration = Duration::from_millis(250);

/// Characters before the statement: `12:03:45.123    2 ms  `.
const PREFIX: usize = 23;

/// Lines kept in the view before it is rebuilt from the (bounded) log.
const MAX_SHOWN: usize = 6000;

/// Filter choices, in dropdown order.
const FILTERS: [&str; 3] = ["All Queries", "Hide Meta Queries", "Your Queries Only"];

pub struct Console {
    pub widget: gtk::Box,
    log: Arc<QueryLog>,
    /// For the highlighting rules (SQL or Redis commands).
    kind: DbKind,
    view: gtk::TextView,
    scroll: gtk::ScrolledWindow,
    filter: gtk::DropDown,
    count: gtk::Label,
    /// Newest entry already looked at (shown or filtered out).
    last_seq: Cell<u64>,
    shown: Cell<usize>,
    polling: Cell<Option<glib::SourceId>>,
}

/// `12:03:45.123` in local time.
fn clock(entry: &LogEntry) -> String {
    let since = entry.at.duration_since(UNIX_EPOCH).unwrap_or_default();
    let time = glib::DateTime::from_unix_local(since.as_secs() as i64)
        .and_then(|t| t.format("%H:%M:%S"))
        .map(|s| s.to_string())
        .unwrap_or_default();
    format!("{time}.{:03}", since.subsec_millis())
}

fn duration(d: Duration) -> String {
    let ms = d.as_secs_f64() * 1000.0;
    if ms < 1.0 {
        "<1 ms".into()
    } else if ms < 1000.0 {
        format!("{ms:.0} ms")
    } else {
        format!("{:.2} s", ms / 1000.0)
    }
}

/// Fold a statement onto one line.
fn one_line(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl Console {
    pub fn new(log: Arc<QueryLog>, kind: DbKind) -> Rc<Self> {
        let title = gtk::Label::builder().label("Console").css_classes(["heading"]).build();
        let count = gtk::Label::builder().css_classes(["dim-label", "caption"]).build();
        let filter = gtk::DropDown::from_strings(&FILTERS);
        filter.set_tooltip_text(Some("Meta queries are the lookups Pueo makes by itself (tables, structure, keys)"));
        filter.add_css_class("flat");
        let clear = gtk::Button::builder()
            .icon_name("edit-clear-all-symbolic")
            .tooltip_text("Clear")
            .css_classes(["flat"])
            .build();
        let close = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text("Close Console (Ctrl+Shift+C)")
            .action_name("win.toggle-console")
            .css_classes(["flat"])
            .build();
        let bar = gtk::Box::builder().spacing(8).css_classes(["console-bar"]).build();
        bar.append(&title);
        bar.append(&count);
        bar.append(&gtk::Box::builder().hexpand(true).build());
        bar.append(&filter);
        bar.append(&clear);
        bar.append(&close);

        let view = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::WordChar)
            .top_margin(6)
            .bottom_margin(8)
            .left_margin(12)
            .right_margin(12)
            .css_classes(["console-text"])
            .build();
        let buffer = view.buffer();
        highlight::create_tags(&buffer);
        let dim = buffer.create_tag(Some("dim"), &[]).expect("new tag");
        let error = buffer.create_tag(Some("error"), &[]).expect("new tag");
        // Text tags can't use theme colours, so follow light/dark by hand (Adwaita's values).
        let style = adw::StyleManager::default();
        let paint = move |style: &adw::StyleManager| {
            let dark = style.is_dark();
            dim.set_foreground(Some(if dark { "#9a9996" } else { "#77767b" }));
            error.set_foreground(Some(if dark { "#ff7b63" } else { "#c01c28" }));
        };
        paint(&style);
        style.connect_dark_notify(paint);
        let scroll = gtk::ScrolledWindow::builder().child(&view).vexpand(true).build();
        // Wrapped statements hang under the SQL column, past the time and duration.
        view.connect_realize(|view| {
            let (width, _) = view.create_pango_layout(Some(&" ".repeat(PREFIX))).pixel_size();
            // A negative indent is a hanging indent: continuation lines move right.
            view.set_indent(-width);
        });

        let widget = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .height_request(110)
            .visible(false)
            .css_classes(["console"])
            .build();
        widget.append(&bar);
        widget.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        widget.append(&scroll);

        let console = Rc::new(Console {
            widget,
            log,
            kind,
            view,
            scroll,
            filter,
            count,
            last_seq: Cell::new(0),
            shown: Cell::new(0),
            polling: Cell::new(None),
        });

        let weak = Rc::downgrade(&console);
        console.filter.connect_selected_notify(move |_| {
            if let Some(c) = weak.upgrade() {
                c.rebuild();
            }
        });
        let weak = Rc::downgrade(&console);
        clear.connect_clicked(move |_| {
            if let Some(c) = weak.upgrade() {
                c.log.clear();
                c.rebuild();
            }
        });
        console
    }

    pub fn is_open(&self) -> bool {
        self.widget.is_visible()
    }

    pub fn set_open(self: &Rc<Self>, open: bool) {
        if open == self.is_open() {
            return;
        }
        self.widget.set_visible(open);
        if let Some(id) = self.polling.take() {
            id.remove();
        }
        if open {
            self.update();
            self.scroll_to_end();
            let weak = Rc::downgrade(self);
            let id = glib::timeout_add_local(POLL, move || match weak.upgrade() {
                Some(c) => {
                    c.update();
                    glib::ControlFlow::Continue
                }
                None => glib::ControlFlow::Break,
            });
            self.polling.set(Some(id));
        }
    }

    fn wanted(&self, kind: LogKind) -> bool {
        match self.filter.selected() {
            1 => kind != LogKind::Meta,
            2 => kind == LogKind::User,
            _ => true,
        }
    }

    /// Start over from what the log still holds (filter changed, cleared, or trimmed).
    fn rebuild(&self) {
        self.view.buffer().set_text("");
        self.last_seq.set(0);
        self.shown.set(0);
        self.count.set_text("");
        self.update();
    }

    /// Append entries recorded since the last update.
    fn update(&self) {
        let entries = self.log.since(self.last_seq.get());
        let Some(last) = entries.last() else { return };
        self.last_seq.set(last.seq);
        let wanted: Vec<&LogEntry> = entries.iter().filter(|e| self.wanted(e.kind)).collect();
        if wanted.is_empty() {
            return;
        }
        let adj = self.scroll.vadjustment();
        let at_end = adj.value() + adj.page_size() >= adj.upper() - 4.0;

        let buffer = self.view.buffer();
        let mut end = buffer.end_iter();
        for entry in &wanted {
            if self.shown.get() > 0 {
                buffer.insert(&mut end, "\n");
            }
            let meta = format!("{}  {:>7}  ", clock(entry), duration(entry.duration));
            buffer.insert_with_tags_by_name(&mut end, &meta, &["dim"]);
            let sql = one_line(&entry.sql);
            let at = end.offset();
            buffer.insert(&mut end, &sql);
            highlight::highlight_at(&buffer, self.kind, at, &sql);
            // Tagging invalidates iterators.
            end = buffer.end_iter();
            if let Some(error) = &entry.error {
                // Lined up under the statement.
                let text = format!("\n{:PREFIX$}{}", "", one_line(error));
                buffer.insert_with_tags_by_name(&mut end, &text, &["error"]);
            }
            self.shown.set(self.shown.get() + 1);
        }
        self.count.set_text(&format!("{} {}", self.shown.get(), if self.shown.get() == 1 { "query" } else { "queries" }));
        if at_end {
            self.scroll_to_end();
        }
        // The log keeps a bounded history; don't let the view grow past it.
        if self.shown.get() > MAX_SHOWN {
            self.rebuild();
        }
    }

    fn scroll_to_end(&self) {
        // After layout, so the new lines are counted.
        let view = self.view.downgrade();
        glib::idle_add_local_once(move || {
            if let Some(view) = view.upgrade() {
                let buffer = view.buffer();
                let mark = buffer.create_mark(None, &buffer.end_iter(), false);
                view.scroll_mark_onscreen(&mark);
                buffer.delete_mark(&mark);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(one_line("SELECT *\n  FROM t\n WHERE a = 1"), "SELECT * FROM t WHERE a = 1");
        assert_eq!(duration(Duration::from_micros(300)), "<1 ms");
        assert_eq!(duration(Duration::from_millis(42)), "42 ms");
        assert_eq!(duration(Duration::from_millis(1500)), "1.50 s");
    }
}
