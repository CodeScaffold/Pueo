//! Create / edit a connection group: name, colour and icon (symbol, emoji or image).

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use super::icons::{self, ColorPicker, GROUP_SYMBOLS};
use crate::config::{self, Group, Icon};

struct Form {
    dialog: adw::Dialog,
    toasts: adw::ToastOverlay,
    name: adw::EntryRow,
    colors: ColorPicker,
    icon: RefCell<Icon>,
    preview: gtk::Box,
}

impl Form {
    fn refresh_preview(&self) {
        while let Some(child) = self.preview.first_child() {
            self.preview.remove(&child);
        }
        self.preview.append(&icons::group_icon(&self.icon.borrow(), self.colors.color(), 64));
    }
}

pub fn show(parent: &impl IsA<gtk::Widget>, existing: Option<Group>, on_save: impl Fn(Group) + 'static) {
    let is_new = existing.is_none();
    let base = existing.unwrap_or_else(|| Group::new(""));

    let preview = gtk::Box::builder().halign(gtk::Align::Center).margin_top(6).margin_bottom(6).build();
    let name = adw::EntryRow::builder().title("Name").text(&base.name).activates_default(true).build();
    let colors = ColorPicker::new(base.color, false);
    let color_row = adw::ActionRow::builder().title("Colour").build();
    color_row.add_suffix(&colors.widget);
    let general = adw::PreferencesGroup::new();
    general.add(&name);
    general.add(&color_row);

    // Icon: symbol grid, emoji, or image.
    let symbols = gtk::FlowBox::builder()
        .max_children_per_line(8)
        .min_children_per_line(8)
        .selection_mode(gtk::SelectionMode::None)
        .row_spacing(4)
        .column_spacing(4)
        .homogeneous(true)
        .build();
    // Only offer symbols the current icon theme actually has.
    let theme = gtk::IconTheme::for_display(&parent.as_ref().display());
    for &sym in GROUP_SYMBOLS.iter().filter(|s| theme.has_icon(s)) {
        let button = gtk::Button::builder()
            .icon_name(sym)
            .tooltip_text(sym.trim_end_matches("-symbolic"))
            .css_classes(["flat", "symbol-choice"])
            .build();
        button.set_widget_name(sym);
        symbols.insert(&button, -1);
    }
    let emoji_button = gtk::MenuButton::builder()
        .label("Pick Emoji…")
        .popover(&gtk::EmojiChooser::new())
        .build();
    let image_button = gtk::Button::with_label("Choose Image…");
    let other_box = gtk::Box::builder().spacing(8).halign(gtk::Align::Center).margin_top(8).build();
    other_box.append(&emoji_button);
    other_box.append(&image_button);

    let icon_group = adw::PreferencesGroup::builder()
        .title("Icon")
        .description("A symbol, an emoji, or your own image")
        .build();
    let icon_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    icon_box.append(&symbols);
    icon_box.append(&other_box);
    icon_group.add(&icon_box);

    let page = adw::PreferencesPage::new();
    let preview_group = adw::PreferencesGroup::new();
    preview_group.add(&preview);
    page.add(&preview_group);
    page.add(&general);
    page.add(&icon_group);

    let cancel = gtk::Button::with_label("Cancel");
    let save = gtk::Button::builder()
        .label(if is_new { "Create" } else { "Save" })
        .css_classes(["suggested-action"])
        .build();
    let header = adw::HeaderBar::builder().show_start_title_buttons(false).show_end_title_buttons(false).build();
    header.pack_start(&cancel);
    header.pack_end(&save);
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&page));
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&toasts));
    let dialog = adw::Dialog::builder()
        .title(if is_new { "New Group" } else { "Edit Group" })
        .content_width(460)
        .content_height(620)
        .child(&toolbar)
        .build();

    let form = Rc::new(Form {
        dialog: dialog.clone(),
        toasts,
        name,
        colors,
        icon: RefCell::new(base.icon.clone()),
        preview,
    });
    form.refresh_preview();

    let weak = Rc::downgrade(&form);
    form.colors.connect_changed(move |_| {
        if let Some(f) = weak.upgrade() {
            f.refresh_preview();
        }
    });

    let mut child = symbols.first_child();
    while let Some(fb_child) = child {
        if let Some(button) = fb_child.first_child().and_downcast::<gtk::Button>() {
            let weak = Rc::downgrade(&form);
            button.connect_clicked(move |b| {
                if let Some(f) = weak.upgrade() {
                    *f.icon.borrow_mut() = Icon::Symbol(b.widget_name().to_string());
                    f.refresh_preview();
                }
            });
        }
        child = fb_child.next_sibling();
    }

    if let Some(chooser) = emoji_button.popover().and_downcast::<gtk::EmojiChooser>() {
        let weak = Rc::downgrade(&form);
        chooser.connect_emoji_picked(move |_, emoji| {
            if let Some(f) = weak.upgrade() {
                *f.icon.borrow_mut() = Icon::Emoji(emoji.to_string());
                f.refresh_preview();
            }
        });
    }

    let weak = Rc::downgrade(&form);
    image_button.connect_clicked(move |b| {
        let Some(f) = weak.upgrade() else { return };
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Images"));
        filter.add_pixbuf_formats();
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let chooser = gtk::FileDialog::builder().title("Choose Group Image").filters(&filters).build();
        let window = b.root().and_downcast::<gtk::Window>();
        let weak = Rc::downgrade(&f);
        chooser.open(window.as_ref(), None::<&gio::Cancellable>, move |res| {
            let (Some(f), Ok(file)) = (weak.upgrade(), res) else { return };
            let Some(path) = file.path() else { return };
            match config::import_icon_image(&path) {
                Ok(copy) => {
                    *f.icon.borrow_mut() = Icon::Image(copy);
                    f.refresh_preview();
                }
                Err(e) => f.toasts.add_toast(adw::Toast::new(&glib::markup_escape_text(&format!("{e}")))),
            }
        });
    });

    let d = dialog.clone();
    cancel.connect_clicked(move |_| {
        d.close();
    });
    let weak = Rc::downgrade(&form);
    save.connect_clicked(move |_| {
        let Some(f) = weak.upgrade() else { return };
        let name = f.name.text().trim().to_string();
        if name.is_empty() {
            f.toasts.add_toast(adw::Toast::new("Give the group a name"));
            return;
        }
        let mut group = base.clone();
        group.name = name;
        group.color = f.colors.color();
        group.icon = f.icon.borrow().clone();
        on_save(group);
        f.dialog.close();
    });

    let form_name = form.name.clone();
    let holder = RefCell::new(Some(form));
    dialog.connect_closed(move |_| {
        holder.borrow_mut().take();
    });
    dialog.set_default_widget(Some(&save));
    dialog.set_focus(Some(&form_name));
    super::pointer_cursors(&dialog);
    dialog.present(Some(parent));
}
