//! The query editor widget and SQL statement splitting.

use crate::db::DbKind;

/// Create the editor: GtkSourceView when built with the `sourceview` feature, else a TextView
/// with Pueo's own highlighter. Both are `gtk::TextView`s.
pub fn create(kind: DbKind) -> gtk::TextView {
    #[cfg(feature = "sourceview")]
    {
        use sourceview5::prelude::*;

        let buffer = sourceview5::Buffer::new(None);
        buffer.set_highlight_matching_brackets(true);
        let language = if kind.is_sql() { sourceview5::LanguageManager::default().language("sql") } else { None };
        match language {
            Some(lang) => buffer.set_language(Some(&lang)),
            None => super::highlight::attach(buffer.upcast_ref(), kind),
        }

        let style = adw::StyleManager::default();
        let apply_scheme = |buffer: &sourceview5::Buffer, dark: bool| {
            let schemes = sourceview5::StyleSchemeManager::default();
            let scheme = if dark {
                schemes.scheme("Adwaita-dark").or_else(|| schemes.scheme("classic-dark"))
            } else {
                schemes.scheme("Adwaita").or_else(|| schemes.scheme("classic"))
            };
            buffer.set_style_scheme(scheme.as_ref());
        };
        apply_scheme(&buffer, style.is_dark());
        let weak = buffer.downgrade();
        style.connect_dark_notify(move |sm| {
            if let Some(b) = weak.upgrade() {
                apply_scheme(&b, sm.is_dark());
            }
        });

        let view = sourceview5::View::with_buffer(&buffer);
        view.set_show_line_numbers(true);
        view.set_highlight_current_line(true);
        view.set_auto_indent(true);
        view.set_tab_width(4);
        view.set_indent_width(4);
        view.set_insert_spaces_instead_of_tabs(true);
        view.upcast()
    }
    #[cfg(not(feature = "sourceview"))]
    {
        let buffer = gtk::TextBuffer::new(None);
        super::highlight::attach(&buffer, kind);
        gtk::TextView::with_buffer(&buffer)
    }
}

/// Character ranges `[start, end)` of the statements in `text`, split on `;` outside of quotes,
/// comments and PostgreSQL dollar-quoted strings. The `;` itself is not included.
pub fn statement_ranges(text: &str) -> Vec<(usize, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < n {
        let c = chars[i];
        match c {
            '\'' | '"' | '`' => {
                i += 1;
                while i < n && chars[i] != c {
                    if chars[i] == '\\' && c != '"' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            '-' if chars.get(i + 1) == Some(&'-') => {
                while i < n && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                while i < n && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                    i += 1;
                }
                i += 1;
            }
            '$' => {
                // $tag$ ... $tag$
                let mut j = i + 1;
                while j < n && (chars[j].is_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
                if j < n && chars[j] == '$' && !(j > i + 1 && chars[i + 1].is_ascii_digit()) {
                    let tag: Vec<char> = chars[i..=j].to_vec();
                    i = j + 1;
                    while i < n && !chars[i..].starts_with(&tag) {
                        i += 1;
                    }
                    i += tag.len() - 1;
                }
            }
            ';' => {
                ranges.push((start, i));
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if start < n {
        ranges.push((start, n));
    }
    ranges
}

/// The statement under the cursor (character offset). A cursor right after `;` or on blank
/// lines following a statement selects that statement.
pub fn statement_at(text: &str, cursor: usize) -> Option<(usize, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let is_blank = |(s, e): (usize, usize)| chars[s..e].iter().all(|c| c.is_whitespace());
    let ranges: Vec<(usize, usize)> = statement_ranges(text);
    let idx = ranges.iter().position(|&(s, e)| cursor >= s && cursor <= e).unwrap_or(ranges.len());
    // Walk back over empty statements (e.g. the cursor sits after the final `;`).
    let mut i = idx.min(ranges.len());
    loop {
        if let Some(&r) = ranges.get(i)
            && !is_blank(r)
        {
            return Some(r);
        }
        if i == 0 {
            return None;
        }
        i -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stmt(text: &str, cursor: usize) -> String {
        let (s, e) = statement_at(text, cursor).unwrap();
        text.chars().skip(s).take(e - s).collect::<String>().trim().to_string()
    }

    #[test]
    fn splits_statements() {
        let sql = "SELECT ';' AS a; -- x;y\nSELECT 2;\n\n";
        assert_eq!(statement_ranges(sql).len(), 3);
        assert_eq!(stmt(sql, 3), "SELECT ';' AS a");
        assert_eq!(stmt(sql, 26), "-- x;y\nSELECT 2");
        assert_eq!(stmt(sql, sql.chars().count()), "-- x;y\nSELECT 2");
        let func = "CREATE FUNCTION f() RETURNS int AS $$ SELECT 1; $$ LANGUAGE sql; SELECT $1";
        assert_eq!(statement_ranges(func).len(), 2);
        assert_eq!(stmt("  SELECT 1", 0), "SELECT 1");
        assert!(statement_at("  \n ", 1).is_none());
    }
}
