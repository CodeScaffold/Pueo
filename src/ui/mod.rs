mod completion;
mod console;
mod connection_dialog;
mod data_grid;
mod editor;
mod filter_bar;
mod group_dialog;
#[cfg_attr(feature = "sourceview", allow(dead_code))]
mod highlight;
mod icons;
mod loading_fx;
mod main_window;
mod picker;
mod query_tab;
mod row_inspector;
mod sidebar;
mod table_tab;
mod welcome;
mod workspace;

pub use main_window::get as main_window;

use gtk::prelude::*;

/// Give everything clickable under `root` the hand cursor: buttons, switches,
/// check boxes and activatable rows (combo, switch and expander rows), but not entry rows.
pub fn pointer_cursors(root: &impl IsA<gtk::Widget>) {
    let mut child = root.as_ref().first_child();
    while let Some(widget) = child {
        let clickable = widget.is::<gtk::Button>()
            || widget.is::<gtk::Switch>()
            || widget.is::<gtk::CheckButton>()
            || widget.is::<gtk::DropDown>()
            || widget.downcast_ref::<gtk::ListBoxRow>().is_some_and(|row| row.is_activatable() && !row.is::<adw::EntryRow>());
        if clickable {
            widget.set_cursor_from_name(Some("pointer"));
        }
        pointer_cursors(&widget);
        child = widget.next_sibling();
    }
}
