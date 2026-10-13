//! The single application window: a rail of open connections on the left,
//! the saved-connection list as the "home" page, and one workspace page per connection.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use super::icons;
use super::welcome::Welcome;
use super::workspace::{self, Workspace};
use crate::config::ConnectionConfig;
use crate::db::log::QueryLog;
use crate::runtime;

/// `win.*` actions forwarded to the active workspace.
const WORKSPACE_ACTIONS: &[&str] =
    &["new-query", "close-tab", "refresh", "find", "save", "insert-row", "stop", "toggle-sidebar", "toggle-inspector", "toggle-console"];

struct Session {
    page: String,
    workspace: Rc<Workspace>,
    button: gtk::ToggleButton,
}

pub struct MainWindow {
    pub window: adw::ApplicationWindow,
    stack: gtk::Stack,
    home_button: gtk::ToggleButton,
    rail_items: gtk::Box,
    welcome: Rc<Welcome>,
    sessions: RefCell<Vec<Session>>,
    counter: Cell<u32>,
    /// Session page whose rail button was right-clicked.
    menu_target: RefCell<Option<String>>,
    menu: gtk::PopoverMenu,
}

thread_local! {
    static INSTANCE: RefCell<Option<Rc<MainWindow>>> = const { RefCell::new(None) };
}

/// The application's window, created on first use.
pub fn get(app: &adw::Application) -> Rc<MainWindow> {
    if let Some(win) = INSTANCE.with_borrow(|i| i.clone()) {
        return win;
    }
    let win = MainWindow::new(app);
    INSTANCE.with_borrow_mut(|i| *i = Some(win.clone()));
    win
}

impl MainWindow {
    fn new(app: &adw::Application) -> Rc<Self> {
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Pueo")
            .default_width(1320)
            .default_height(820)
            .build();

        // ---- Rail ----
        let home_button = gtk::ToggleButton::builder()
            .child(&gtk::Image::builder().icon_name("view-app-grid-symbolic").pixel_size(20).build())
            .tooltip_text("Connections (Ctrl+0)")
            .active(true)
            .width_request(54)
            .height_request(54)
            .halign(gtk::Align::Center)
            .css_classes(["rail-home"])
            .build();
        let rail_items = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();
        let rail_scroll = gtk::ScrolledWindow::builder()
            .child(&rail_items)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        let add = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("New Connection (Ctrl+N)")
            .action_name("win.new-connection")
            .css_classes(["flat", "circular"])
            .halign(gtk::Align::Center)
            .build();
        let rail = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .css_classes(["conn-rail"])
            .build();
        rail.append(&home_button);
        rail.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        rail.append(&rail_scroll);
        rail.append(&add);
        // Dragging the rail moves the window, like a header bar.
        let rail_handle = gtk::WindowHandle::builder().child(&rail).build();

        // ---- Pages ----
        let welcome = Welcome::new();
        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .transition_duration(120)
            .hexpand(true)
            .build();
        stack.add_named(&welcome.widget, Some("home"));

        let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        body.append(&rail_handle);
        body.append(&stack);
        window.set_content(Some(&body));

        let menu_model = gio::Menu::new();
        menu_model.append(Some("New Query"), Some("win.rail-new-query"));
        menu_model.append(Some("Close Connection"), Some("win.rail-close"));
        let menu = gtk::PopoverMenu::from_model(Some(&menu_model));
        menu.set_has_arrow(true);
        menu.set_position(gtk::PositionType::Right);
        // Not under the WindowHandle, whose drag gesture would swallow clicks in the menu.
        menu.set_parent(&body);

        let win = Rc::new(MainWindow {
            window,
            stack,
            home_button,
            rail_items,
            welcome,
            sessions: RefCell::new(Vec::new()),
            counter: Cell::new(0),
            menu_target: RefCell::new(None),
            menu,
        });
        win.setup();
        win
    }

    fn setup(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.welcome.connect_open(move |cfg, done| {
            if let Some(w) = weak.upgrade() {
                w.open_connection(cfg, done);
            }
        });

        let weak = Rc::downgrade(self);
        self.home_button.connect_toggled(move |b| {
            if let (Some(w), true) = (weak.upgrade(), b.is_active()) {
                w.show_page("home");
            }
        });

        let add = |name: &str, f: Box<dyn Fn(&Rc<Self>)>| {
            let action = gio::SimpleAction::new(name, None);
            let weak = Rc::downgrade(self);
            action.connect_activate(move |_, _| {
                if let Some(w) = weak.upgrade() {
                    f(&w);
                }
            });
            self.window.add_action(&action);
        };
        add("new-connection", Box::new(|w| {
            w.home_button.set_active(true);
            w.welcome.new_connection();
        }));
        add("close-connection", Box::new(|w| {
            if let Some(page) = w.active_page() {
                w.close_session(&page);
            }
        }));
        add("rail-close", Box::new(|w| {
            if let Some(page) = w.menu_target.borrow_mut().take() {
                w.close_session(&page);
            }
        }));
        add("rail-new-query", Box::new(|w| {
            if let Some(page) = w.menu_target.borrow_mut().take() {
                w.select(&page);
                w.forward("new-query");
            }
        }));
        for &name in WORKSPACE_ACTIONS {
            add(name, Box::new(move |w| w.forward(name)));
        }

        // Ctrl+0 = connections, Ctrl+1…9 = open connections in rail order.
        let switch = gio::SimpleAction::new("switch-connection", Some(glib::VariantTy::INT32));
        let weak = Rc::downgrade(self);
        switch.connect_activate(move |_, param| {
            let (Some(w), Some(n)) = (weak.upgrade(), param.and_then(|p| p.get::<i32>())) else { return };
            if n == 0 {
                w.home_button.set_active(true);
            } else if let Some(page) = w.sessions.borrow().get(n as usize - 1).map(|s| s.page.clone()) {
                w.select(&page);
            }
        });
        self.window.add_action(&switch);

        // Confirm before quitting with uncommitted edits.
        let weak = Rc::downgrade(self);
        self.window.connect_close_request(move |window| {
            let Some(w) = weak.upgrade() else { return glib::Propagation::Proceed };
            let dirty: Vec<String> = w
                .sessions
                .borrow()
                .iter()
                .filter(|s| s.workspace.has_changes())
                .map(|s| s.workspace.config().display_name())
                .collect();
            if dirty.is_empty() {
                w.shutdown_all();
                return glib::Propagation::Proceed;
            }
            let dialog = adw::AlertDialog::new(
                Some("Quit With Uncommitted Changes?"),
                Some(&format!("Pending edits in {} will be lost.", dirty.join(", "))),
            );
            dialog.add_responses(&[("cancel", "Cancel"), ("quit", "Quit")]);
            dialog.set_response_appearance("quit", adw::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
            dialog.present(Some(window));
            let (weak, window) = (Rc::downgrade(&w), window.clone());
            dialog.connect_response(None, move |_, r| {
                if r == "quit" {
                    if let Some(w) = weak.upgrade() {
                        w.shutdown_all();
                    }
                    window.destroy();
                }
            });
            glib::Propagation::Stop
        });
        self.update_actions();
    }

    fn shutdown_all(&self) {
        for s in self.sessions.borrow().iter() {
            s.workspace.shutdown();
        }
        self.sessions.borrow_mut().clear();
        INSTANCE.with_borrow_mut(|i| *i = None);
    }

    fn active_page(&self) -> Option<String> {
        self.stack.visible_child_name().map(|n| n.to_string()).filter(|n| n != "home")
    }

    fn active_workspace(&self) -> Option<Rc<Workspace>> {
        let page = self.active_page()?;
        self.sessions.borrow().iter().find(|s| s.page == page).map(|s| s.workspace.clone())
    }

    fn forward(&self, action: &str) {
        match self.active_workspace() {
            Some(ws) => ws.activate(action),
            None if action == "find" => self.welcome.focus_search(),
            None => {}
        }
    }

    /// Workspace shortcuts only make sense while a connection is shown.
    fn update_actions(&self) {
        let has_ws = self.active_page().is_some();
        for name in WORKSPACE_ACTIONS.iter().copied().chain(["close-connection"]) {
            if name == "find" {
                continue;
            }
            if let Some(a) = self.window.lookup_action(name).and_downcast::<gio::SimpleAction>() {
                a.set_enabled(has_ws);
            }
        }
    }

    fn show_page(&self, page: &str) {
        self.stack.set_visible_child_name(page);
        let title = self
            .active_workspace()
            .map(|ws| format!("{} - Pueo", ws.config().display_name()))
            .unwrap_or_else(|| "Pueo".into());
        self.window.set_title(Some(&title));
        self.update_actions();
    }

    fn select(&self, page: &str) {
        if let Some(s) = self.sessions.borrow().iter().find(|s| s.page == page) {
            s.button.set_active(true);
        }
    }

    pub fn present(&self) {
        self.window.present();
    }

    /// Connect (or switch to the already open connection) and show it.
    pub fn open_connection(self: &Rc<Self>, cfg: ConnectionConfig, done: Box<dyn FnOnce()>) {
        let existing = self.sessions.borrow().iter().find(|s| s.workspace.config().id == cfg.id).map(|s| s.page.clone());
        if let Some(page) = existing {
            self.select(&page);
            done();
            return;
        }
        let weak = Rc::downgrade(self);
        let log = QueryLog::new();
        runtime::spawn(workspace::open_driver(cfg.clone(), log.clone()), move |res| {
            done();
            let Some(w) = weak.upgrade() else { return };
            match res {
                Ok((driver, info)) => {
                    let ws = Workspace::new(cfg.clone(), driver, info, log);
                    w.add_session(cfg, ws);
                }
                Err(e) => {
                    let dialog = adw::AlertDialog::new(Some("Connection Failed"), Some(&format!("{e:#}")));
                    dialog.add_response("ok", "OK");
                    dialog.present(Some(&w.window));
                }
            }
        });
    }

    fn add_session(self: &Rc<Self>, cfg: ConnectionConfig, workspace: Rc<Workspace>) {
        let n = self.counter.get() + 1;
        self.counter.set(n);
        let page = format!("session-{n}");
        self.stack.add_named(&workspace.widget, Some(&page));

        let badge = icons::avatar(&cfg);
        badge.set_halign(gtk::Align::Center);
        badge.add_css_class("rail-avatar");
        let name = gtk::Label::builder()
            .label(icons::strip_db_extension(&cfg.display_name()))
            .max_width_chars(7)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["rail-name"])
            .build();
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(3)
            .valign(gtk::Align::Center)
            .halign(gtk::Align::Center)
            .build();
        content.append(&badge);
        content.append(&name);
        let button = gtk::ToggleButton::builder()
            .child(&content)
            .group(&self.home_button)
            .tooltip_markup(format!(
                "<b>{}</b>\n{} · {}",
                glib::markup_escape_text(&cfg.display_name()),
                cfg.kind.label(),
                glib::markup_escape_text(&cfg.summary())
            ))
            .width_request(54)
            .height_request(54)
            .halign(gtk::Align::Center)
            .css_classes(["rail-item"])
            .build();

        if let Some(color) = cfg.effective_color() {
            button.add_css_class(&format!("color-{}", color.name()));
        }

        let weak = Rc::downgrade(self);
        let p = page.clone();
        button.connect_toggled(move |b| {
            if let (Some(w), true) = (weak.upgrade(), b.is_active()) {
                w.show_page(&p);
            }
        });

        // Right-click: menu; middle-click: close.
        let click = gtk::GestureClick::builder().button(0).build();
        let weak = Rc::downgrade(self);
        let p = page.clone();
        click.connect_pressed(move |gesture, _, x, y| {
            let Some(w) = weak.upgrade() else { return };
            match gesture.current_button() {
                gdk::BUTTON_SECONDARY => {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    *w.menu_target.borrow_mut() = Some(p.clone());
                    if let Some(widget) = gesture.widget()
                        && let Some(pt) = widget.compute_point(
                            &w.menu.parent().unwrap(),
                            &gtk::graphene::Point::new(x as f32, y as f32),
                        )
                    {
                        w.menu.set_pointing_to(Some(&gdk::Rectangle::new(pt.x() as i32, pt.y() as i32, 1, 1)));
                    }
                    w.menu.popup();
                }
                gdk::BUTTON_MIDDLE => {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    w.close_session(&p);
                }
                _ => {}
            }
        });
        button.add_controller(click);

        self.rail_items.append(&button);
        self.sessions.borrow_mut().push(Session { page, workspace, button: button.clone() });
        button.set_active(true);
    }

    fn close_session(self: &Rc<Self>, page: &str) {
        let Some(ws) = self.sessions.borrow().iter().find(|s| s.page == page).map(|s| s.workspace.clone()) else {
            return;
        };
        if ws.has_changes() {
            let dialog = adw::AlertDialog::new(
                Some("Close Connection?"),
                Some(&format!("“{}” has uncommitted changes that will be lost.", ws.config().display_name())),
            );
            dialog.add_responses(&[("cancel", "Cancel"), ("close", "Close")]);
            dialog.set_response_appearance("close", adw::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
            let weak = Rc::downgrade(self);
            let page = page.to_string();
            dialog.connect_response(None, move |_, r| {
                if let (Some(w), "close") = (weak.upgrade(), r) {
                    w.remove_session(&page);
                }
            });
            dialog.present(Some(&self.window));
        } else {
            self.remove_session(page);
        }
    }

    fn remove_session(&self, page: &str) {
        let (idx, session) = {
            let mut sessions = self.sessions.borrow_mut();
            let Some(idx) = sessions.iter().position(|s| s.page == page) else { return };
            (idx, sessions.remove(idx))
        };
        let was_active = session.button.is_active();
        session.workspace.shutdown();
        self.rail_items.remove(&session.button);
        self.stack.remove(&session.workspace.widget);
        if was_active {
            // Show the neighbour, or the connection list when none is left.
            let next = {
                let sessions = self.sessions.borrow();
                sessions.get(idx).or_else(|| sessions.last()).map(|s| s.button.clone())
            };
            match next {
                Some(b) => b.set_active(true),
                None => self.home_button.set_active(true),
            }
        }
        self.update_actions();
    }
}

impl Drop for MainWindow {
    fn drop(&mut self) {
        self.menu.unparent();
    }
}
