//! Lightweight syntax highlighting using `GtkTextTag`s: the query editor when GtkSourceView
//! is not available, and the console.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::db::DbKind;

const SQL_KEYWORDS: &[&str] = &[
    "ADD", "ALL", "ALTER", "AND", "ANY", "AS", "ASC", "BEGIN", "BETWEEN", "BY", "CASCADE", "CASE",
    "CAST", "CHECK", "COLUMN", "COMMIT", "CONSTRAINT", "CREATE", "CROSS", "DATABASE", "DEFAULT",
    "DELETE", "DESC", "DISTINCT", "DROP", "ELSE", "END", "EXCEPT", "EXISTS", "EXPLAIN", "FALSE",
    "FETCH", "FOREIGN", "FROM", "FULL", "FUNCTION", "GRANT", "GROUP", "HAVING", "IF", "ILIKE", "IN",
    "INDEX", "INNER", "INSERT", "INTERSECT", "INTO", "IS", "JOIN", "KEY", "LEFT", "LIKE", "LIMIT",
    "NOT", "NULL", "OFFSET", "ON", "OR", "ORDER", "OUTER", "OVER", "PARTITION", "PRIMARY",
    "REFERENCES", "RENAME", "REPLACE", "RETURNING", "REVOKE", "RIGHT", "ROLLBACK", "SCHEMA",
    "SELECT", "SET", "SHOW", "TABLE", "THEN", "TO", "TRANSACTION", "TRUE", "TRUNCATE", "UNION",
    "UNIQUE", "UPDATE", "USE", "USING", "VALUES", "VIEW", "WHEN", "WHERE", "WITH", "ANALYZE",
    "VACUUM", "PRAGMA", "DESCRIBE", "RECURSIVE", "LATERAL", "NATURAL", "COUNT", "SUM", "AVG", "MIN",
    "MAX", "COALESCE", "INTEGER", "INT", "BIGINT", "TEXT", "VARCHAR", "BOOLEAN", "TIMESTAMP",
    "DATE", "SERIAL", "NUMERIC", "JSON", "JSONB", "UUID",
];

struct Palette {
    keyword: &'static str,
    string: &'static str,
    number: &'static str,
    comment: &'static str,
}

const LIGHT: Palette = Palette { keyword: "#1c71d8", string: "#26a269", number: "#c64600", comment: "#77767b" };
const DARK: Palette = Palette { keyword: "#78aeed", string: "#8ff0a4", number: "#ffbe6f", comment: "#9a9996" };

#[derive(Clone, Copy, PartialEq)]
enum Token {
    Keyword,
    String,
    Number,
    Comment,
}

impl Token {
    fn tag(self) -> &'static str {
        match self {
            Token::Keyword => "hl-keyword",
            Token::String => "hl-string",
            Token::Number => "hl-number",
            Token::Comment => "hl-comment",
        }
    }
}

/// Returns (token, start_char, end_char) spans.
fn lex(text: &str, kind: DbKind) -> Vec<(Token, i32, i32)> {
    let chars: Vec<char> = text.chars().collect();
    let mut spans = Vec::new();
    let mut i = 0;
    let mut line_start = true;
    while i < chars.len() {
        let c = chars[i];
        let start = i;
        if c == '\n' {
            line_start = true;
            i += 1;
            continue;
        }
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        let at_line_start = std::mem::replace(&mut line_start, false);

        if (c == '-' && chars.get(i + 1) == Some(&'-')) || (c == '#' && kind != DbKind::Postgres) {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            spans.push((Token::Comment, start as i32, i as i32));
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i = (i + 2).min(chars.len());
            spans.push((Token::Comment, start as i32, i as i32));
        } else if c == '\'' || (c == '"' && kind == DbKind::Redis) {
            i += 1;
            while i < chars.len() && chars[i] != c {
                if chars[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            i = (i + 1).min(chars.len());
            spans.push((Token::String, start as i32, i as i32));
        } else if c == '"' || c == '`' {
            // A quoted identifier (a table or column), even when it is spelled like a keyword.
            i += 1;
            while i < chars.len() && chars[i] != c {
                i += 1;
            }
            i = (i + 1).min(chars.len());
        } else if c.is_ascii_digit() {
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '.') {
                i += 1;
            }
            spans.push((Token::Number, start as i32, i as i32));
        } else if c.is_alphabetic() || c == '_' {
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect::<String>().to_ascii_uppercase();
            let is_keyword = match kind {
                DbKind::Redis => at_line_start,
                _ => SQL_KEYWORDS.contains(&word.as_str()),
            };
            if is_keyword {
                spans.push((Token::Keyword, start as i32, i as i32));
            }
        } else {
            i += 1;
        }
    }
    spans
}

fn apply_palette(buffer: &gtk::TextBuffer, dark: bool) {
    let p = if dark { &DARK } else { &LIGHT };
    let table = buffer.tag_table();
    for (name, color) in [
        ("hl-keyword", p.keyword),
        ("hl-string", p.string),
        ("hl-number", p.number),
        ("hl-comment", p.comment),
    ] {
        if let Some(tag) = table.lookup(name) {
            tag.set_foreground(Some(color));
        }
    }
}

fn highlight(buffer: &gtk::TextBuffer, kind: DbKind) {
    let (start, end) = buffer.bounds();
    for t in [Token::Keyword, Token::String, Token::Number, Token::Comment] {
        buffer.remove_tag_by_name(t.tag(), &start, &end);
    }
    let text = buffer.text(&start, &end, true);
    highlight_at(buffer, kind, 0, &text);
}

/// Add the highlighting tags to `buffer`, coloured for the current light/dark style.
pub fn create_tags(buffer: &gtk::TextBuffer) {
    buffer.create_tag(Some("hl-keyword"), &[("weight", &700i32)]);
    buffer.create_tag(Some("hl-string"), &[]);
    buffer.create_tag(Some("hl-number"), &[]);
    buffer.create_tag(Some("hl-comment"), &[("style", &gtk::pango::Style::Italic)]);

    let style = adw::StyleManager::default();
    apply_palette(buffer, style.is_dark());
    let weak_buffer = buffer.downgrade();
    style.connect_dark_notify(move |sm| {
        if let Some(b) = weak_buffer.upgrade() {
            apply_palette(&b, sm.is_dark());
        }
    });
}

/// Highlight `text`, which was just inserted into `buffer` at character `offset`.
pub fn highlight_at(buffer: &gtk::TextBuffer, kind: DbKind, offset: i32, text: &str) {
    for (token, s, e) in lex(text, kind) {
        let a = buffer.iter_at_offset(offset + s);
        let b = buffer.iter_at_offset(offset + e);
        buffer.apply_tag_by_name(token.tag(), &a, &b);
    }
}

/// Highlight the editor as it is typed in.
pub fn attach(buffer: &gtk::TextBuffer, kind: DbKind) {
    create_tags(buffer);

    // Re-highlight once per main-loop iteration at most.
    let pending = Rc::new(Cell::new(false));
    buffer.connect_changed(move |buffer| {
        if pending.replace(true) {
            return;
        }
        let pending = pending.clone();
        let weak = buffer.downgrade();
        glib::idle_add_local_once(move || {
            pending.set(false);
            if let Some(b) = weak.upgrade() {
                highlight(&b, kind);
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexes_sql() {
        let spans = lex("SELECT 'a--b', 42 -- note\nFROM t", DbKind::Postgres);
        let kinds: Vec<_> = spans.iter().map(|s| s.0).collect();
        assert!(kinds == vec![Token::Keyword, Token::String, Token::Number, Token::Comment, Token::Keyword]);
    }

    #[test]
    fn quoted_identifiers_are_not_keywords() {
        let spans = lex(r#"SELECT * FROM "order" ORDER BY `key` DESC"#, DbKind::Postgres);
        let words: Vec<i32> = spans.iter().filter(|s| s.0 == Token::Keyword).map(|s| s.1).collect();
        // SELECT, FROM, ORDER, BY, DESC; not "order" or `key`.
        assert_eq!(words.len(), 5);
    }
}
