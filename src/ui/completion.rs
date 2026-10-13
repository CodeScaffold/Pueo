//! Autocompletion popover for the query editor: keywords, tables and columns (alias-aware).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, glib};

use crate::db::DbKind;

/// Table and column names of the current database, shared by all query tabs.
#[derive(Debug, Default)]
pub struct SchemaCache {
    pub tables: Vec<String>,
    pub columns: Vec<(String, String)>,
}

pub type SharedSchema = Rc<RefCell<SchemaCache>>;

const SQL_WORDS: &[&str] = &[
    "SELECT", "FROM", "WHERE", "AND", "OR", "NOT", "NULL", "IS", "IN", "LIKE", "ILIKE", "BETWEEN",
    "EXISTS", "INSERT", "INTO", "VALUES", "UPDATE", "SET", "DELETE", "CREATE", "TABLE", "VIEW",
    "INDEX", "DROP", "ALTER", "ADD", "COLUMN", "PRIMARY", "KEY", "FOREIGN", "REFERENCES", "UNIQUE",
    "DEFAULT", "JOIN", "INNER", "LEFT", "RIGHT", "FULL", "OUTER", "CROSS", "ON", "USING", "GROUP",
    "BY", "ORDER", "ASC", "DESC", "HAVING", "LIMIT", "OFFSET", "UNION", "ALL", "DISTINCT", "AS",
    "CASE", "WHEN", "THEN", "ELSE", "END", "BEGIN", "COMMIT", "ROLLBACK", "TRANSACTION", "WITH",
    "RETURNING", "COUNT", "SUM", "AVG", "MIN", "MAX", "COALESCE", "CAST", "EXPLAIN", "ANALYZE",
    "TRUNCATE", "SHOW", "DESCRIBE", "CURRENT_TIMESTAMP", "NOW", "TRUE", "FALSE",
];

const REDIS_WORDS: &[&str] = &[
    "GET", "SET", "DEL", "EXISTS", "EXPIRE", "TTL", "PERSIST", "TYPE", "KEYS", "SCAN", "RENAME",
    "INCR", "INCRBY", "DECR", "APPEND", "MGET", "MSET", "HGET", "HSET", "HDEL", "HGETALL", "HKEYS",
    "HVALS", "HLEN", "HINCRBY", "LPUSH", "RPUSH", "LPOP", "RPOP", "LRANGE", "LLEN", "LSET", "LREM",
    "SADD", "SREM", "SMEMBERS", "SISMEMBER", "SCARD", "ZADD", "ZREM", "ZRANGE", "ZREVRANGE",
    "ZSCORE", "ZCARD", "ZINCRBY", "XADD", "XRANGE", "XLEN", "INFO", "DBSIZE", "SELECT", "FLUSHDB",
    "PING", "OBJECT", "MEMORY", "CONFIG", "CLIENT", "PUBLISH", "WITHSCORES", "MATCH", "COUNT", "EX",
];

const TABLE_INTRODUCERS: &[&str] = &["FROM", "JOIN", "UPDATE", "INTO", "TABLE"];
const MAX_ITEMS: usize = 40;

#[derive(Clone)]
struct Candidate {
    text: String,
    detail: &'static str,
}

pub struct Completion {
    view: gtk::TextView,
    kind: DbKind,
    schema: SharedSchema,
    popover: gtk::Popover,
    scroll: gtk::ScrolledWindow,
    list: gtk::ListBox,
    items: RefCell<Vec<Candidate>>,
    /// Character offset where the word being completed starts.
    word_start: Cell<i32>,
    /// Set when the last buffer change was typing a single identifier character.
    typed: Cell<bool>,
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

fn strip_quotes(s: &str) -> String {
    s.trim_matches(|c| c == '"' || c == '`' || c == '[' || c == ']').to_string()
}

/// Map aliases and table names mentioned after FROM/JOIN/... to table names.
fn referenced_tables(text: &str) -> HashMap<String, String> {
    let tokens: Vec<String> = text
        .split(|c: char| c.is_whitespace() || c == ',' || c == '(' || c == ')' || c == ';')
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect();
    let mut map = HashMap::new();
    for i in 0..tokens.len() {
        if !TABLE_INTRODUCERS.contains(&tokens[i].to_ascii_uppercase().as_str()) {
            continue;
        }
        let Some(raw) = tokens.get(i + 1) else { continue };
        let table = strip_quotes(raw.rsplit('.').next().unwrap_or(raw));
        map.insert(table.to_lowercase(), table.clone());
        let mut j = i + 2;
        if tokens.get(j).is_some_and(|t| t.eq_ignore_ascii_case("AS")) {
            j += 1;
        }
        if let Some(alias) = tokens.get(j) {
            let upper = alias.to_ascii_uppercase();
            if !SQL_WORDS.contains(&upper.as_str()) && alias.chars().all(is_word_char) {
                map.insert(alias.to_lowercase(), table);
            }
        }
    }
    map
}

impl Completion {
    pub fn attach(view: &gtk::TextView, kind: DbKind, schema: SharedSchema) -> Rc<Self> {
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Browse)
            .css_classes(["completion-list"])
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .child(&list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .max_content_height(260)
            .propagate_natural_height(true)
            .min_content_width(260)
            .build();
        let popover = gtk::Popover::builder()
            .child(&scroll)
            .autohide(false)
            .has_arrow(false)
            .can_focus(false)
            .position(gtk::PositionType::Bottom)
            .halign(gtk::Align::Start)
            .css_classes(["completion"])
            .build();
        popover.set_parent(view);
        view.connect_destroy(glib::clone!(
            #[weak]
            popover,
            move |_| popover.unparent()
        ));

        let c = Rc::new(Completion {
            view: view.clone(),
            kind,
            schema,
            popover,
            scroll,
            list,
            items: RefCell::new(Vec::new()),
            word_start: Cell::new(0),
            typed: Cell::new(false),
        });

        let buffer = view.buffer();
        let weak = Rc::downgrade(&c);
        buffer.connect_insert_text(move |_, _, text| {
            if let Some(c) = weak.upgrade() {
                let mut chars = text.chars();
                c.typed.set(matches!((chars.next(), chars.next()), (Some(ch), None) if is_word_char(ch) || ch == '.'));
            }
        });
        let weak = Rc::downgrade(&c);
        buffer.connect_changed(move |_| {
            let Some(c) = weak.upgrade() else { return };
            let typed = c.typed.replace(false);
            if typed || c.popover.is_visible() {
                let weak = Rc::downgrade(&c);
                glib::idle_add_local_once(move || {
                    if let Some(c) = weak.upgrade() {
                        c.update(false);
                    }
                });
            }
        });

        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&c);
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(c) = weak.upgrade() else { return glib::Propagation::Proceed };
            if key == gdk::Key::space && state.contains(gdk::ModifierType::CONTROL_MASK) {
                c.update(true);
                return glib::Propagation::Stop;
            }
            if !c.popover.is_visible() {
                return glib::Propagation::Proceed;
            }
            let plain = !state.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK);
            match key {
                gdk::Key::Escape => c.hide(),
                gdk::Key::Down | gdk::Key::KP_Down => c.move_selection(1),
                gdk::Key::Up | gdk::Key::KP_Up => c.move_selection(-1),
                gdk::Key::Page_Down => c.move_selection(8),
                gdk::Key::Page_Up => c.move_selection(-8),
                gdk::Key::Tab | gdk::Key::Return | gdk::Key::KP_Enter if plain => c.accept_selected(),
                _ => {
                    if matches!(key, gdk::Key::Left | gdk::Key::Right | gdk::Key::Home | gdk::Key::End) {
                        c.hide();
                    }
                    return glib::Propagation::Proceed;
                }
            }
            glib::Propagation::Stop
        });
        view.add_controller(keys);

        let focus = gtk::EventControllerFocus::new();
        let weak = Rc::downgrade(&c);
        focus.connect_leave(move |_| {
            if let Some(c) = weak.upgrade() {
                c.hide();
            }
        });
        view.add_controller(focus);

        let click = gtk::GestureClick::new();
        let weak = Rc::downgrade(&c);
        click.connect_pressed(move |_, _, _, _| {
            if let Some(c) = weak.upgrade() {
                c.hide();
            }
        });
        view.add_controller(click);

        let weak = Rc::downgrade(&c);
        c.list.connect_row_activated(move |_, row| {
            if let Some(c) = weak.upgrade() {
                c.list.select_row(Some(row));
                c.accept_selected();
            }
        });
        c
    }

    fn hide(&self) {
        self.popover.popdown();
    }

    fn move_selection(&self, delta: i32) {
        let n = self.items.borrow().len() as i32;
        if n == 0 {
            return;
        }
        let cur = self.list.selected_row().map_or(0, |r| r.index());
        let next = (cur + delta).clamp(0, n - 1);
        if let Some(row) = self.list.row_at_index(next) {
            self.list.select_row(Some(&row));
            if let Some(bounds) = row.compute_bounds(&self.list) {
                let adj = self.scroll.vadjustment();
                let (top, bottom) = (bounds.y() as f64, (bounds.y() + bounds.height()) as f64);
                if top < adj.value() {
                    adj.set_value(top);
                } else if bottom > adj.value() + adj.page_size() {
                    adj.set_value(bottom - adj.page_size());
                }
            }
        }
    }

    fn accept_selected(&self) {
        let idx = self.list.selected_row().map_or(0, |r| r.index()) as usize;
        let Some(item) = self.items.borrow().get(idx).cloned() else { return };
        let buffer = self.view.buffer();
        let mut start = buffer.iter_at_offset(self.word_start.get());
        let mut end = buffer.iter_at_mark(&buffer.get_insert());
        buffer.begin_user_action();
        buffer.delete(&mut start, &mut end);
        buffer.insert(&mut start, &item.text);
        buffer.end_user_action();
        self.hide();
    }

    /// Recompute candidates for the word at the cursor. `manual` = Ctrl+Space.
    fn update(&self, manual: bool) {
        let buffer = self.view.buffer();
        let cursor = buffer.iter_at_mark(&buffer.get_insert());
        let (start, end) = buffer.bounds();
        let text = buffer.text(&start, &end, false).to_string();
        let before: Vec<char> = buffer.text(&start, &cursor, false).chars().collect();

        let mut i = before.len();
        while i > 0 && is_word_char(before[i - 1]) {
            i -= 1;
        }
        let prefix: String = before[i..].iter().collect();
        self.word_start.set(i as i32);

        // A qualifier like `alias.` before the word.
        let qualifier = (i > 0 && before[i - 1] == '.').then(|| {
            let mut j = i - 1;
            while j > 0 && (is_word_char(before[j - 1]) || before[j - 1] == '"' || before[j - 1] == '`') {
                j -= 1;
            }
            strip_quotes(&before[j..i - 1].iter().collect::<String>())
        });

        if qualifier.is_none() && prefix.chars().count() < if manual { 0 } else { 2 } {
            self.hide();
            return;
        }
        let candidates = self.candidates(&text, &prefix, qualifier.as_deref());
        let exact_only = candidates.len() == 1 && candidates[0].text == prefix;
        if candidates.is_empty() || exact_only {
            self.hide();
            return;
        }
        self.show(candidates, &cursor);
    }

    fn candidates(&self, text: &str, prefix: &str, qualifier: Option<&str>) -> Vec<Candidate> {
        let schema = self.schema.borrow();
        let lower = prefix.to_lowercase();
        let matches = |s: &str| s.to_lowercase().starts_with(&lower) && s.len() >= prefix.len();
        let referenced = referenced_tables(text);
        let mut out: Vec<Candidate> = Vec::new();
        let push = |text: &str, detail: &'static str, out: &mut Vec<Candidate>| {
            if matches(text) && !out.iter().any(|c| c.text == text) {
                out.push(Candidate { text: text.to_string(), detail });
            }
        };

        if let Some(q) = qualifier {
            let table = referenced.get(&q.to_lowercase()).cloned().unwrap_or_else(|| q.to_string());
            for (t, col) in &schema.columns {
                if t.eq_ignore_ascii_case(&table) {
                    push(col, "column", &mut out);
                }
            }
            out.truncate(MAX_ITEMS);
            return out;
        }

        if self.kind.is_sql() {
            let tables: Vec<&String> = referenced.values().collect();
            for (t, col) in &schema.columns {
                if tables.iter().any(|r| r.eq_ignore_ascii_case(t)) {
                    push(col, "column", &mut out);
                }
            }
            for t in &schema.tables {
                push(t, "table", &mut out);
            }
        }
        let lowercase = prefix.chars().all(|c| !c.is_uppercase());
        let words = if self.kind.is_sql() { SQL_WORDS } else { REDIS_WORDS };
        for w in words {
            let w = if lowercase { w.to_lowercase() } else { w.to_string() };
            push(&w, "keyword", &mut out);
        }
        out.truncate(MAX_ITEMS);
        out
    }

    fn show(&self, items: Vec<Candidate>, at: &gtk::TextIter) {
        self.list.remove_all();
        for item in &items {
            let row = gtk::Box::builder().spacing(12).margin_start(6).margin_end(6).build();
            row.append(
                &gtk::Label::builder()
                    .label(&item.text)
                    .xalign(0.0)
                    .hexpand(true)
                    .css_classes(["monospace"])
                    .build(),
            );
            row.append(&gtk::Label::builder().label(item.detail).css_classes(["dim-label", "caption"]).build());
            self.list.append(&row);
        }
        if let Some(first) = self.list.row_at_index(0) {
            self.list.select_row(Some(&first));
        }
        self.scroll.vadjustment().set_value(0.0);
        *self.items.borrow_mut() = items;

        let rect = self.view.iter_location(at);
        let (x, y) =
            self.view.buffer_to_window_coords(gtk::TextWindowType::Widget, rect.x(), rect.y() + rect.height());
        self.popover.set_pointing_to(Some(&gdk::Rectangle::new(x, y, 1, 1)));
        if !self.popover.is_visible() {
            self.popover.popup();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::referenced_tables;

    #[test]
    fn resolves_aliases() {
        let m = referenced_tables("SELECT o. FROM public.orders AS o JOIN \"customers\" c ON c.id = o.cid");
        assert_eq!(m.get("o").unwrap(), "orders");
        assert_eq!(m.get("c").unwrap(), "customers");
        assert_eq!(m.get("orders").unwrap(), "orders");
    }
}
