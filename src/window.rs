use std::sync::Arc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};

use crate::db::{self, Connection, ConnectionConfig};
use crate::main_view::MainView;
use crate::store::{self, SavedConnection};

mod imp {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Debug, Default, gtk::CompositeTemplate)]
    #[template(resource = "/com/marwa/sqweel/window.ui")]
    pub struct SqweelWindow {
        #[template_child]
        pub nav: TemplateChild<adw::NavigationView>,
        #[template_child]
        pub saved_group: TemplateChild<adw::PreferencesGroup>,
        #[template_child]
        pub toast_overlay: TemplateChild<adw::ToastOverlay>,
        #[template_child]
        pub type_row: TemplateChild<adw::ComboRow>,
        #[template_child]
        pub host_row: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub port_row: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub database_row: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub username_row: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub password_row: TemplateChild<adw::PasswordEntryRow>,
        #[template_child]
        pub ssl_row: TemplateChild<adw::SwitchRow>,
        #[template_child]
        pub save_row: TemplateChild<adw::SwitchRow>,
        #[template_child]
        pub connect_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub test_button: TemplateChild<gtk::Button>,

        /// Prevents overlapping connect/test requests.
        pub busy: Cell<bool>,
        /// Currently displayed saved-connection rows (for clearing on refresh).
        pub saved_rows: RefCell<Vec<adw::ActionRow>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SqweelWindow {
        const NAME: &'static str = "SqweelWindow";
        type Type = super::SqweelWindow;
        type ParentType = adw::ApplicationWindow;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
            klass.bind_template_instance_callbacks();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for SqweelWindow {
        fn constructed(&self) {
            self.parent_constructed();
            // Sensible defaults to mirror the mockup.
            self.port_row.set_text("5432");
            self.obj().refresh_saved();
        }
    }
    impl WidgetImpl for SqweelWindow {}
    impl WindowImpl for SqweelWindow {}
    impl ApplicationWindowImpl for SqweelWindow {}
    impl AdwApplicationWindowImpl for SqweelWindow {}
}

glib::wrapper! {
    pub struct SqweelWindow(ObjectSubclass<imp::SqweelWindow>)
        @extends adw::ApplicationWindow, gtk::ApplicationWindow, gtk::Window, gtk::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
                    gtk::ConstraintTarget, gtk::Native, gtk::Root, gtk::ShortcutManager;
}

#[gtk::template_callbacks]
impl SqweelWindow {
    pub fn new(app: &adw::Application) -> Self {
        glib::Object::builder().property("application", app).build()
    }

    /// Read the form into a driver-agnostic config, validating as we go.
    fn collect(&self) -> Result<ConnectionConfig, String> {
        let imp = self.imp();

        // Only PostgreSQL is offered for now; map the dropdown index to a driver.
        let driver_id = match imp.type_row.selected() {
            0 => "postgres",
            _ => return Err("Unsupported database type".into()),
        };

        let host = imp.host_row.text().trim().to_string();
        if host.is_empty() {
            return Err("Host is required".into());
        }

        let port_text = imp.port_row.text();
        let port: u16 = port_text
            .trim()
            .parse()
            .map_err(|_| "Port must be a number between 1 and 65535".to_string())?;

        Ok(ConnectionConfig {
            driver_id: driver_id.to_string(),
            host,
            port,
            database: imp.database_row.text().trim().to_string(),
            username: imp.username_row.text().trim().to_string(),
            password: imp.password_row.text().to_string(),
            ssl: imp.ssl_row.is_active(),
        })
    }

    fn toast(&self, message: &str) {
        self.imp()
            .toast_overlay
            .add_toast(adw::Toast::new(message));
    }

    fn set_busy(&self, busy: bool) {
        let imp = self.imp();
        imp.busy.set(busy);
        imp.connect_button.set_sensitive(!busy);
        imp.test_button.set_sensitive(!busy);
    }

    #[template_callback]
    fn on_connect_clicked(&self) {
        if self.imp().busy.get() {
            return;
        }
        let cfg = match self.collect() {
            Ok(cfg) => cfg,
            Err(e) => return self.toast(&e),
        };
        let Some(driver) = db::driver(&cfg.driver_id) else {
            return self.toast("Unknown database driver");
        };

        self.set_busy(true);
        let cfg_for_view = cfg.clone();
        let save = self.imp().save_row.is_active();
        let rx = crate::runtime::spawn(async move { driver.connect(&cfg).await });

        let this = self.clone();
        glib::spawn_future_local(async move {
            let result = rx.recv().await;
            this.set_busy(false);
            match result {
                Ok(Ok(conn)) => {
                    // Persist only after a successful connect, so we never
                    // save unusable credentials.
                    if save {
                        store::upsert(
                            SavedConnection::from_config(&cfg_for_view),
                            &cfg_for_view.password,
                        );
                        this.refresh_saved();
                    }
                    let conn: Arc<dyn Connection> = Arc::from(conn);
                    let page = MainView::new(conn, &cfg_for_view);
                    this.imp().nav.push(&page);
                }
                Ok(Err(e)) => this.toast(&e.to_string()),
                Err(_) => this.toast("Connection task was dropped"),
            }
        });
    }

    /// Rebuild the saved-connections list from disk.
    fn refresh_saved(&self) {
        let imp = self.imp();
        for row in imp.saved_rows.take() {
            imp.saved_group.remove(&row);
        }

        let saved = store::load();
        imp.saved_group.set_visible(!saved.is_empty());

        let mut rows = Vec::new();
        for conn in saved {
            let row = adw::ActionRow::builder()
                .title(&conn.name)
                .subtitle(&conn.subtitle())
                .activatable(true)
                .build();

            let icon = gtk::Image::from_icon_name("network-server-symbolic");
            row.add_prefix(&icon);

            let delete = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .valign(gtk::Align::Center)
                .tooltip_text("Forget")
                .css_classes(["flat"])
                .build();
            delete.connect_clicked(glib::clone!(
                #[weak(rename_to = this)]
                self,
                #[strong]
                conn,
                move |_| {
                    store::remove(&conn.name);
                    this.refresh_saved();
                }
            ));
            row.add_suffix(&delete);

            row.connect_activated(glib::clone!(
                #[weak(rename_to = this)]
                self,
                #[strong]
                conn,
                move |_| this.connect_saved(&conn)
            ));

            imp.saved_group.add(&row);
            rows.push(row);
        }
        imp.saved_rows.replace(rows);
    }

    /// Fill the form from a saved connection (password from keyring) and connect.
    fn connect_saved(&self, conn: &SavedConnection) {
        let imp = self.imp();
        imp.type_row.set_selected(0); // PostgreSQL only for now
        imp.host_row.set_text(&conn.host);
        imp.port_row.set_text(&conn.port.to_string());
        imp.database_row.set_text(&conn.database);
        imp.username_row.set_text(&conn.username);
        imp.password_row.set_text(&store::get_password(&conn.name));
        imp.ssl_row.set_active(conn.ssl);
        self.on_connect_clicked();
    }

    #[template_callback]
    fn on_test_clicked(&self) {
        if self.imp().busy.get() {
            return;
        }
        let cfg = match self.collect() {
            Ok(cfg) => cfg,
            Err(e) => return self.toast(&e),
        };
        let Some(driver) = db::driver(&cfg.driver_id) else {
            return self.toast("Unknown database driver");
        };

        self.set_busy(true);
        let rx = crate::runtime::spawn(async move { driver.test(&cfg).await });

        let this = self.clone();
        glib::spawn_future_local(async move {
            let result = rx.recv().await;
            this.set_busy(false);
            match result {
                Ok(Ok(())) => this.toast("Connection successful"),
                Ok(Err(e)) => this.toast(&e.to_string()),
                Err(_) => this.toast("Test task was dropped"),
            }
        });
    }
}
