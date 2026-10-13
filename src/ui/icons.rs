//! Group icons (symbol / emoji / image) and the colour-swatch picker.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;

use crate::config::{Color, ConnectionConfig, Icon};

/// Symbolic icons offered for groups.
pub const GROUP_SYMBOLS: &[&str] = &[
    "folder-symbolic",
    "user-home-symbolic",
    "network-server-symbolic",
    "computer-symbolic",
    "drive-harddisk-symbolic",
    "network-workgroup-symbolic",
    "system-users-symbolic",
    "starred-symbolic",
    "emblem-important-symbolic",
    "security-high-symbolic",
    "applications-science-symbolic",
    "applications-engineering-symbolic",
    "applications-games-symbolic",
    "applications-graphics-symbolic",
    "applications-multimedia-symbolic",
    "applications-utilities-symbolic",
    "preferences-system-symbolic",
    "utilities-terminal-symbolic",
    "mail-unread-symbolic",
    "weather-clear-symbolic",
    "emblem-favorite-symbolic",
    "document-open-recent-symbolic",
    "view-grid-symbolic",
    "media-record-symbolic",
];

/// A square icon tile of `size` pixels for a group.
pub fn group_icon(icon: &Icon, color: Option<Color>, size: i32) -> gtk::Widget {
    let color = color.unwrap_or(Color::Blue);
    let tile = gtk::Box::builder()
        .width_request(size)
        .height_request(size)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .hexpand(false)
        .vexpand(false)
        .overflow(gtk::Overflow::Hidden)
        .css_classes(["group-icon"])
        .build();
    // `hexpand(false)` above stops the children's expansion from reaching the row.
    // A missing or unreadable image falls back to the folder symbol below.
    let texture = match icon {
        Icon::Image(path) => square_texture(path, size * 2),
        _ => None,
    };
    match (icon, texture) {
        (_, Some(texture)) => {
            tile.append(&gtk::Image::builder().paintable(&texture).pixel_size(size).build());
        }
        (Icon::Emoji(emoji), None) => {
            tile.add_css_class(&format!("soft-{}", color.name()));
            let label = gtk::Label::builder().label(emoji).hexpand(true).halign(gtk::Align::Center).build();
            // Scale the emoji with the tile.
            let attrs = gtk::pango::AttrList::new();
            attrs.insert(gtk::pango::AttrFloat::new_scale(size as f64 / 26.0));
            label.set_attributes(Some(&attrs));
            tile.append(&label);
        }
        (other, None) => {
            tile.add_css_class(&format!("solid-{}", color.name()));
            let name = match other {
                Icon::Symbol(name) => name.as_str(),
                _ => "folder-symbolic",
            };
            tile.append(
                &gtk::Image::builder()
                    .icon_name(name)
                    .pixel_size(size * 55 / 100)
                    .hexpand(true)
                    .halign(gtk::Align::Center)
                    .build(),
            );
        }
    }
    tile.upcast()
}

/// Load an image, crop it to a centred square and scale it to `px` pixels.
fn square_texture(path: &str, px: i32) -> Option<gtk::gdk::Texture> {
    use gtk::gdk_pixbuf::{InterpType, Pixbuf};
    let pixbuf = Pixbuf::from_file(path).ok()?;
    let side = pixbuf.width().min(pixbuf.height());
    let square = pixbuf.new_subpixbuf((pixbuf.width() - side) / 2, (pixbuf.height() - side) / 2, side, side);
    let scaled = square.scale_simple(px, px, InterpType::Bilinear)?;
    Some(gtk::gdk::Texture::for_pixbuf(&scaled))
}

/// A row of round colour swatches. `allow_none` adds a "no colour" swatch.
pub struct ColorPicker {
    pub widget: gtk::Box,
    selected: Rc<Cell<Option<Color>>>,
}

impl ColorPicker {
    pub fn new(current: Option<Color>, allow_none: bool) -> Self {
        let widget = gtk::Box::builder().spacing(6).valign(gtk::Align::Center).build();
        let selected = Rc::new(Cell::new(current));
        let mut first: Option<gtk::ToggleButton> = None;
        let mut options: Vec<Option<Color>> = Vec::new();
        if allow_none {
            options.push(None);
        }
        options.extend(Color::ALL.map(Some));
        for option in options {
            let button = gtk::ToggleButton::builder()
                .tooltip_text(option.map_or("No colour", Color::label))
                .css_classes(["swatch"])
                .active(option == current)
                .build();
            match option {
                Some(c) => button.add_css_class(&format!("solid-{}", c.name())),
                None => {
                    button.add_css_class("swatch-none");
                    button.set_icon_name("action-unavailable-symbolic");
                }
            }
            match &first {
                Some(f) => button.set_group(Some(f)),
                None => first = Some(button.clone()),
            }
            let sel = selected.clone();
            button.connect_toggled(move |b| {
                if b.is_active() {
                    sel.set(option);
                }
            });
            widget.append(&button);
        }
        if current.is_none() && !allow_none {
            if let Some(f) = &first {
                f.set_active(true);
            }
        }
        ColorPicker { widget, selected }
    }

    pub fn color(&self) -> Option<Color> {
        self.selected.get()
    }

    /// Called whenever the choice changes.
    pub fn connect_changed(&self, f: impl Fn(Option<Color>) + 'static) {
        let f = Rc::new(f);
        let mut child = self.widget.first_child();
        while let Some(w) = child {
            if let Some(b) = w.downcast_ref::<gtk::ToggleButton>() {
                let (f, sel) = (f.clone(), self.selected.clone());
                b.connect_toggled(move |b| {
                    if b.is_active() {
                        f(sel.get());
                    }
                });
            }
            child = w.next_sibling();
        }
    }
}

/// The round avatar identifying a connection: its initials on its colour
/// (or the database-type colour when it has none).
pub fn avatar(cfg: &ConnectionConfig) -> gtk::Label {
    let label = gtk::Label::builder()
        .label(initials(&cfg.display_name()))
        .valign(gtk::Align::Center)
        .css_classes(["avatar-circle"])
        .build();
    match cfg.effective_color() {
        Some(color) => label.add_css_class(&format!("solid-{}", color.name())),
        None => label.add_css_class(cfg.kind.css_class()),
    }
    label
}

/// "shop.db" → "shop"; other names are unchanged.
pub fn strip_db_extension(name: &str) -> &str {
    name.rsplit_once('.').map_or(name, |(stem, ext)| {
        if ["db", "sqlite", "sqlite3", "db3"].contains(&ext.to_lowercase().as_str()) { stem } else { name }
    })
}

/// Up to two letters for the rail avatar: "Local Postgres" → "LP", "ERP" → "ER", "shop.db" → "SH".
pub fn initials(name: &str) -> String {
    let stem = strip_db_extension(name);
    let words: Vec<&str> = stem.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let letters: String = match words.as_slice() {
        [] => "?".into(),
        [one] => one.chars().take(2).collect(),
        [first, second, ..] => first.chars().take(1).chain(second.chars().take(1)).collect(),
    };
    letters.to_uppercase()
}

#[cfg(test)]
mod tests {
    use super::initials;

    #[test]
    fn makes_initials() {
        assert_eq!(initials("Local Postgres"), "LP");
        assert_eq!(initials("ERP"), "ER");
        assert_eq!(initials("shop.db"), "SH");
        assert_eq!(initials("prod-analytics_eu"), "PA");
        assert_eq!(initials("x"), "X");
        assert_eq!(initials("..."), "?");
        assert_eq!(initials("été 2026"), "É2");
    }
}
