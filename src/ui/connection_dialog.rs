use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use super::icons::ColorPicker;
use crate::config::{ConnectionConfig, Group, SshAuth, Tag};
use crate::db::{self, DbKind};
use crate::{runtime, secrets};

struct Form {
    dialog: adw::Dialog,
    status: StatusLine,
    base: RefCell<ConnectionConfig>,
    kind_buttons: Vec<gtk::ToggleButton>,
    name: adw::EntryRow,
    host: adw::EntryRow,
    port: adw::SpinRow,
    user: adw::EntryRow,
    password: adw::PasswordEntryRow,
    database: adw::EntryRow,
    schema: adw::EntryRow,
    tls: adw::SwitchRow,
    file: adw::EntryRow,
    server_group: adw::PreferencesGroup,
    file_group: adw::PreferencesGroup,
    ssh_group: adw::PreferencesGroup,
    ssh_row: adw::ExpanderRow,
    ssh_host: adw::EntryRow,
    ssh_port: adw::SpinRow,
    ssh_user: adw::EntryRow,
    ssh_auth: adw::ComboRow,
    ssh_secret: adw::PasswordEntryRow,
    ssh_key: adw::EntryRow,
    test_button: gtk::Button,
    colors: ColorPicker,
    tag_row: adw::ComboRow,
    group_row: adw::ComboRow,
    groups: Vec<Group>,
    current_kind: Cell<DbKind>,
}

/// What the footer's status line shows.
enum Status<'a> {
    /// The keyring note.
    Idle,
    Busy(&'a str),
    Success(&'a str),
    Error(&'a str),
}

/// The footer's status line: test results and form problems, colour-coded.
struct StatusLine {
    widget: gtk::Box,
    icon: gtk::Image,
    spinner: adw::Spinner,
    label: gtk::Label,
}

impl StatusLine {
    fn new() -> Self {
        let icon = gtk::Image::new();
        let spinner = adw::Spinner::builder().visible(false).build();
        let label = gtk::Label::builder().ellipsize(gtk::pango::EllipsizeMode::End).xalign(0.0).build();
        let widget = gtk::Box::builder()
            .spacing(8)
            .hexpand(true)
            .halign(gtk::Align::End)
            .valign(gtk::Align::Center)
            .css_classes(["test-status"])
            .build();
        widget.append(&icon);
        widget.append(&spinner);
        widget.append(&label);
        let line = StatusLine { widget, icon, spinner, label };
        line.set(Status::Idle);
        line
    }

    fn set(&self, status: Status) {
        for class in ["idle", "busy", "success", "error"] {
            self.widget.remove_css_class(class);
        }
        let (class, icon, text) = match status {
            Status::Idle => ("idle", None, "Passwords are stored in the system keyring"),
            Status::Busy(text) => ("busy", None, text),
            Status::Success(text) => ("success", Some("object-select-symbolic"), text),
            Status::Error(text) => ("error", Some("dialog-error-symbolic"), text),
        };
        self.widget.add_css_class(class);
        self.icon.set_icon_name(icon);
        self.icon.set_visible(icon.is_some());
        self.spinner.set_visible(class == "busy");
        self.label.set_label(text);
        // Long errors are cut off; the tooltip has the full text.
        self.widget.set_tooltip_text((class == "error" || class == "success").then_some(text));
    }
}

impl Form {
    fn kind(&self) -> DbKind {
        self.kind_buttons.iter().position(|b| b.is_active()).map_or(DbKind::Postgres, |i| DbKind::ALL[i])
    }

    fn ssh_auth(&self) -> SshAuth {
        SshAuth::ALL[self.ssh_auth.selected() as usize]
    }

    /// The config plus the database password and the SSH password / key passphrase.
    fn collect(&self) -> (ConnectionConfig, String, String) {
        let mut cfg = self.base.borrow().clone();
        cfg.kind = self.kind();
        cfg.name = self.name.text().trim().to_string();
        cfg.host = self.host.text().trim().to_string();
        cfg.port = self.port.value() as u16;
        cfg.user = self.user.text().trim().to_string();
        cfg.database = self.database.text().trim().to_string();
        cfg.schema = self.schema.text().trim().to_string();
        cfg.use_tls = self.tls.is_active();
        cfg.file_path = self.file.text().trim().to_string();
        cfg.ssh.enabled = self.ssh_row.enables_expansion() && cfg.kind != DbKind::Sqlite;
        cfg.ssh.host = self.ssh_host.text().trim().to_string();
        cfg.ssh.port = self.ssh_port.value() as u16;
        cfg.ssh.user = self.ssh_user.text().trim().to_string();
        cfg.ssh.auth = self.ssh_auth();
        cfg.ssh.key_path = self.ssh_key.text().trim().to_string();
        cfg.color = self.colors.color();
        cfg.tag = (self.tag_row.selected() as usize).checked_sub(1).map(|i| Tag::ALL[i]);
        cfg.group = (self.group_row.selected() as usize).checked_sub(1).and_then(|i| self.groups.get(i)).map(|g| g.id.clone());
        let ssh_secret = if cfg.ssh.auth == SshAuth::Agent { String::new() } else { self.ssh_secret.text().to_string() };
        (cfg, self.password.text().to_string(), ssh_secret)
    }

    fn update_for_kind(&self) {
        let kind = self.kind();
        let previous = self.current_kind.replace(kind);
        if self.port.value() as u16 == previous.default_port() || self.port.value() == 0.0 {
            self.port.set_value(kind.default_port() as f64);
        }
        let sqlite = kind == DbKind::Sqlite;
        self.server_group.set_visible(!sqlite);
        self.file_group.set_visible(sqlite);
        self.database.set_title(if kind == DbKind::Redis { "Database Index" } else { "Database" });
        self.user.set_title(if kind == DbKind::Redis { "Username (ACL, optional)" } else { "User" });
        self.ssh_group.set_visible(!sqlite);
        self.schema.set_visible(kind == DbKind::Postgres);
    }

    fn update_for_auth(&self) {
        let auth = self.ssh_auth();
        self.ssh_secret.set_visible(auth != SshAuth::Agent);
        self.ssh_secret.set_title(if auth == SshAuth::Key { "Key Passphrase (optional)" } else { "SSH Password" });
        self.ssh_key.set_visible(auth == SshAuth::Key);
    }
}

/// Show the new/edit connection dialog. `existing` may be a prefilled template when `is_new`. `on_save` receives the config, the database password
/// and the SSH password (or key passphrase).
pub fn show(
    parent: &impl IsA<gtk::Widget>,
    existing: Option<ConnectionConfig>,
    is_new: bool,
    groups: &[Group],
    on_save: impl Fn(ConnectionConfig, String, String) + 'static,
) {
    let is_new = is_new || existing.is_none();
    let base = existing.unwrap_or_else(|| ConnectionConfig::new(DbKind::Postgres));

    // Database type: one tile per kind.
    let kinds = gtk::Box::builder().spacing(10).homogeneous(true).build();
    let mut kind_buttons: Vec<gtk::ToggleButton> = Vec::new();
    for kind in DbKind::ALL {
        let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();
        content.append(
            &gtk::Image::builder()
                .icon_name(kind.icon_name())
                .pixel_size(20)
                .halign(gtk::Align::Center)
                .css_classes(["db-badge", "db-logo", kind.css_class()])
                .build(),
        );
        content.append(&gtk::Label::builder().label(kind.label()).css_classes(["caption-heading"]).build());
        let button = gtk::ToggleButton::builder()
            .child(&content)
            .active(kind == base.kind)
            .css_classes(["kind-tile"])
            .build();
        if let Some(first) = kind_buttons.first() {
            button.set_group(Some(first));
        }
        kinds.append(&button);
        kind_buttons.push(button);
    }
    let kind_group = adw::PreferencesGroup::builder().title("Type").build();
    kind_group.add(&kinds);

    let name = adw::EntryRow::builder().title("Name").text(&base.name).build();
    let general = adw::PreferencesGroup::new();
    general.add(&name);

    let host = adw::EntryRow::builder().title("Host").text(&base.host).build();
    let port = adw::SpinRow::with_range(0.0, 65535.0, 1.0);
    port.set_title("Port");
    port.set_value(base.port as f64);
    let user = adw::EntryRow::builder().title("User").text(&base.user).build();
    let password = adw::PasswordEntryRow::builder().title("Password").build();
    let database = adw::EntryRow::builder().title("Database").text(&base.database).build();
    let schema = adw::EntryRow::builder().title("Default Schema (empty = public)").text(&base.schema).build();
    let tls = adw::SwitchRow::builder()
        .title("Use SSL/TLS")
        .subtitle("Encrypt the connection (certificate not verified)")
        .active(base.use_tls)
        .build();
    let server_group = adw::PreferencesGroup::builder().title("Server").build();
    for row in [host.upcast_ref::<gtk::Widget>(), port.upcast_ref(), user.upcast_ref(), password.upcast_ref(), database.upcast_ref(), schema.upcast_ref(), tls.upcast_ref()] {
        server_group.add(row);
    }

    let file = adw::EntryRow::builder().title("Database File").text(&base.file_path).build();
    let browse = gtk::Button::builder()
        .icon_name("document-open-symbolic")
        .valign(gtk::Align::Center)
        .tooltip_text("Choose File")
        .css_classes(["flat"])
        .build();
    file.add_suffix(&browse);
    let file_group = adw::PreferencesGroup::builder()
        .title("File")
        .description("An existing SQLite database, or a new path to create one")
        .build();
    file_group.add(&file);

    // SSH tunnel.
    let ssh_row = adw::ExpanderRow::builder()
        .title("SSH Tunnel")
        .subtitle("Connect through a bastion host using the system ssh client")
        .show_enable_switch(true)
        .enable_expansion(base.ssh.enabled)
        .expanded(base.ssh.enabled)
        .build();
    let ssh_host = adw::EntryRow::builder().title("SSH Host").text(&base.ssh.host).build();
    let ssh_port = adw::SpinRow::with_range(1.0, 65535.0, 1.0);
    ssh_port.set_title("SSH Port");
    ssh_port.set_value(base.ssh.port as f64);
    let ssh_user = adw::EntryRow::builder().title("SSH User").text(&base.ssh.user).build();
    let auths = gtk::StringList::new(&SshAuth::ALL.map(|a| a.label()));
    let ssh_auth = adw::ComboRow::builder().title("Authentication").model(&auths).build();
    ssh_auth.set_selected(SshAuth::ALL.iter().position(|a| *a == base.ssh.auth).unwrap_or(0) as u32);
    let ssh_secret = adw::PasswordEntryRow::builder().title("SSH Password").build();
    let ssh_key = adw::EntryRow::builder().title("Private Key").text(&base.ssh.key_path).build();
    let browse_key = gtk::Button::builder()
        .icon_name("document-open-symbolic")
        .valign(gtk::Align::Center)
        .tooltip_text("Choose Key")
        .css_classes(["flat"])
        .build();
    ssh_key.add_suffix(&browse_key);
    for row in [ssh_host.upcast_ref::<gtk::Widget>(), ssh_port.upcast_ref(), ssh_user.upcast_ref(), ssh_auth.upcast_ref(), ssh_secret.upcast_ref(), ssh_key.upcast_ref()] {
        ssh_row.add_row(row);
    }
    let ssh_group = adw::PreferencesGroup::builder()
        .description("The database host and port above are resolved from the SSH server")
        .build();
    ssh_group.add(&ssh_row);

    // Appearance on the home page.
    let colors = ColorPicker::new(base.color, true);
    let color_row = adw::ActionRow::builder().title("Colour").build();
    color_row.add_suffix(&colors.widget);
    let tag_names: Vec<&str> = std::iter::once("None").chain(Tag::ALL.iter().map(|t| t.label())).collect();
    let tag_row = adw::ComboRow::builder()
        .title("Tag")
        .subtitle("Environment label; also colours the row if no colour is set")
        .model(&gtk::StringList::new(&tag_names))
        .build();
    tag_row.set_selected(base.tag.and_then(|t| Tag::ALL.iter().position(|x| *x == t)).map_or(0, |i| i as u32 + 1));
    let group_names: Vec<&str> = std::iter::once("Ungrouped").chain(groups.iter().map(|g| g.name.as_str())).collect();
    let group_row = adw::ComboRow::builder().title("Group").model(&gtk::StringList::new(&group_names)).build();
    group_row.set_selected(
        base.group.as_ref().and_then(|id| groups.iter().position(|g| &g.id == id)).map_or(0, |i| i as u32 + 1),
    );
    let appearance = adw::PreferencesGroup::builder().title("Appearance").build();
    appearance.add(&color_row);
    appearance.add(&tag_row);
    appearance.add(&group_row);

    // What to connect to first; how it looks on the home page last.
    let page = adw::PreferencesPage::new();
    page.add(&kind_group);
    page.add(&general);
    page.add(&server_group);
    page.add(&file_group);
    page.add(&ssh_group);
    page.add(&appearance);

    let test_button = gtk::Button::builder().label("Test Connection").build();
    let status = StatusLine::new();
    let footer = gtk::Box::builder()
        .spacing(12)
        .margin_top(10)
        .margin_bottom(10)
        .margin_start(24)
        .margin_end(24)
        .build();
    footer.append(&test_button);
    footer.append(&status.widget);

    let cancel = gtk::Button::with_label("Cancel");
    let save = gtk::Button::builder()
        .label(if is_new { "Create" } else { "Save" })
        .css_classes(["suggested-action"])
        .build();
    let header = adw::HeaderBar::builder()
        .show_start_title_buttons(false)
        .show_end_title_buttons(false)
        .build();
    header.pack_start(&cancel);
    header.pack_end(&save);

    let toolbar = adw::ToolbarView::builder().bottom_bar_style(adw::ToolbarStyle::Raised).build();
    toolbar.add_top_bar(&header);
    toolbar.add_bottom_bar(&footer);
    toolbar.set_content(Some(&page));

    let dialog = adw::Dialog::builder()
        .title(if is_new { "New Connection" } else { "Edit Connection" })
        .content_width(620)
        .content_height(800)
        .width_request(360)
        .height_request(320)
        .child(&toolbar)
        .build();

    let form = Rc::new(Form {
        dialog: dialog.clone(),
        status,
        current_kind: Cell::new(base.kind),
        base: RefCell::new(base.clone()),
        kind_buttons,
        name,
        host,
        port,
        user,
        password,
        database,
        schema,
        tls,
        file,
        server_group,
        file_group,
        ssh_group,
        ssh_row,
        ssh_host,
        ssh_port,
        ssh_user,
        ssh_auth,
        ssh_secret,
        ssh_key,
        test_button,
        colors,
        tag_row,
        group_row,
        groups: groups.to_vec(),
    });
    form.update_for_kind();
    form.update_for_auth();

    // Prefill the stored password when editing.
    if !is_new {
        let weak = Rc::downgrade(&form);
        let id = base.id.clone();
        runtime::spawn(
            async move {
                let db = secrets::get_password(&id, secrets::Slot::Database).await.ok().flatten();
                let ssh = secrets::get_password(&id, secrets::Slot::Ssh).await.ok().flatten();
                (db, ssh)
            },
            move |(db, ssh)| {
                let Some(f) = weak.upgrade() else { return };
                if let Some(pw) = db {
                    f.password.set_text(&pw);
                }
                if let Some(pw) = ssh {
                    f.ssh_secret.set_text(&pw);
                }
            },
        );
    }

    for button in &form.kind_buttons {
        let weak = Rc::downgrade(&form);
        button.connect_toggled(move |b| {
            if let (Some(f), true) = (weak.upgrade(), b.is_active()) {
                f.update_for_kind();
            }
        });
    }

    let weak = Rc::downgrade(&form);
    form.ssh_auth.connect_selected_notify(move |_| {
        if let Some(f) = weak.upgrade() {
            f.update_for_auth();
        }
    });

    let weak = Rc::downgrade(&form);
    browse_key.connect_clicked(move |b| {
        let Some(f) = weak.upgrade() else { return };
        let chooser = gtk::FileDialog::builder().title("Choose SSH Private Key").build();
        chooser.set_initial_folder(Some(&gio::File::for_path(glib::home_dir().join(".ssh"))));
        let window = b.root().and_downcast::<gtk::Window>();
        let weak = Rc::downgrade(&f);
        chooser.open(window.as_ref(), None::<&gio::Cancellable>, move |res| {
            if let (Some(f), Ok(file)) = (weak.upgrade(), res)
                && let Some(path) = file.path()
            {
                f.ssh_key.set_text(&path.to_string_lossy());
            }
        });
    });

    let weak = Rc::downgrade(&form);
    browse.connect_clicked(move |b| {
        let Some(f) = weak.upgrade() else { return };
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("SQLite databases"));
        for pat in ["*.db", "*.sqlite", "*.sqlite3", "*.db3"] {
            filter.add_pattern(pat);
        }
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let chooser = gtk::FileDialog::builder().title("Open SQLite Database").filters(&filters).build();
        let window = b.root().and_downcast::<gtk::Window>();
        let weak = Rc::downgrade(&f);
        chooser.open(window.as_ref(), None::<&gio::Cancellable>, move |res| {
            if let (Some(f), Ok(file)) = (weak.upgrade(), res)
                && let Some(path) = file.path()
            {
                f.file.set_text(&path.to_string_lossy());
                if f.name.text().is_empty()
                    && let Some(stem) = path.file_stem()
                {
                    f.name.set_text(&stem.to_string_lossy());
                }
            }
        });
    });

    let weak = Rc::downgrade(&form);
    form.test_button.connect_clicked(move |button| {
        let Some(f) = weak.upgrade() else { return };
        let (cfg, pw, ssh_pw) = f.collect();
        button.set_sensitive(false);
        f.status.set(Status::Busy("Connecting…"));
        let weak = Rc::downgrade(&f);
        runtime::spawn(
            async move {
                let driver = db::connect(&cfg, Some(pw), Some(ssh_pw)).await?;
                driver.server_info().await
            },
            move |res| {
                let Some(f) = weak.upgrade() else { return };
                f.test_button.set_sensitive(true);
                match res {
                    Ok(info) => f.status.set(Status::Success(&format!("Connected · {info}"))),
                    Err(e) => f.status.set(Status::Error(&format!("{e:#}"))),
                }
            },
        );
    });

    let d = dialog.clone();
    cancel.connect_clicked(move |_| {
        d.close();
    });

    let weak = Rc::downgrade(&form);
    save.connect_clicked(move |_| {
        let Some(f) = weak.upgrade() else { return };
        let (cfg, pw, ssh_pw) = f.collect();
        if cfg.ssh.enabled && cfg.ssh.host.is_empty() {
            f.status.set(Status::Error("Enter the SSH host"));
            return;
        }
        if cfg.kind == DbKind::Sqlite && cfg.file_path.is_empty() {
            f.status.set(Status::Error("Choose a database file"));
            return;
        }
        on_save(cfg, pw, ssh_pw);
        f.dialog.close();
    });

    // The form lives as long as the dialog.
    let holder = RefCell::new(Some(form));
    dialog.connect_closed(move |_| {
        holder.borrow_mut().take();
    });
    super::pointer_cursors(&dialog);
    dialog.present(Some(parent));
}
