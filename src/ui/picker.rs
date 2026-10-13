//! A searchable popover list used to pick a database or a schema.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

pub struct Picker {
    pub popover: gtk::Popover,
    list: gtk::ListBox,
    search: gtk::SearchEntry,
    row_icon: &'static str,
    on_pick: RefCell<Option<Rc<dyn Fn(String)>>>,
}

impl Picker {
    pub fn new(title: &str, placeholder: &str, row_icon: &'static str) -> Rc<Self> {
        let search = gtk::SearchEntry::builder().placeholder_text(placeholder).build();
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["navigation-sidebar"])
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .child(&list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(360)
            .build();
        let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).width_request(260).build();
        content.append(&gtk::Label::builder().label(title).xalign(0.0).margin_start(4).css_classes(["heading"]).build());
        content.append(&search);
        content.append(&scroll);
        let popover = gtk::Popover::builder().child(&content).build();

        let picker = Rc::new(Picker { popover, list, search, row_icon, on_pick: RefCell::new(None) });

        let weak = Rc::downgrade(&picker);
        picker.list.connect_row_activated(move |_, row| {
            let Some(p) = weak.upgrade() else { return };
            p.popover.popdown();
            let name = row.widget_name().to_string();
            let cb = p.on_pick.borrow().clone();
            if let Some(cb) = cb {
                cb(name);
            }
        });
        let search = picker.search.downgrade();
        picker.list.set_filter_func(move |row| {
            let needle = search.upgrade().map(|s| s.text().to_lowercase()).unwrap_or_default();
            needle.is_empty() || row.widget_name().to_lowercase().contains(&needle)
        });
        let weak = Rc::downgrade(&picker);
        picker.search.connect_search_changed(move |_| {
            if let Some(p) = weak.upgrade() {
                p.list.invalidate_filter();
            }
        });
        // Enter picks the first match.
        let weak = Rc::downgrade(&picker);
        picker.search.connect_activate(move |_| {
            let Some(p) = weak.upgrade() else { return };
            // Enter can arrive before the debounced search-changed; filter now.
            p.list.invalidate_filter();
            let mut i = 0;
            while let Some(row) = p.list.row_at_index(i) {
                if row.is_child_visible() {
                    row.activate();
                    break;
                }
                i += 1;
            }
        });
        let weak = Rc::downgrade(&picker);
        picker.popover.connect_show(move |_| {
            if let Some(p) = weak.upgrade() {
                p.search.set_text("");
                p.search.grab_focus();
            }
        });
        picker
    }

    pub fn connect_pick(&self, f: impl Fn(String) + 'static) {
        *self.on_pick.borrow_mut() = Some(Rc::new(f));
    }

    /// Replace the choices; `current` gets a check mark.
    pub fn set_items(&self, names: &[String], current: &str) {
        self.list.remove_all();
        for name in names {
            let row_box = gtk::Box::builder().spacing(8).build();
            row_box.append(&gtk::Image::from_icon_name(self.row_icon));
            row_box.append(
                &gtk::Label::builder()
                    .label(name)
                    .xalign(0.0)
                    .hexpand(true)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .build(),
            );
            let check = gtk::Image::from_icon_name("object-select-symbolic");
            check.set_opacity(if name == current { 1.0 } else { 0.0 });
            row_box.append(&check);
            self.list.append(&gtk::ListBoxRow::builder().child(&row_box).name(name).build());
        }
    }
}
