use std::sync::Arc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};

use crate::db::{self, Connection, ConnectionConfig};
use crate::main_view::MainView;

mod imp {
    use super::*;
    use std::cell::Cell;

    #[derive(Debug, Default, gtk::CompositeTemplate)]
    #[template(resource = "/com/marwa/sqweel/window.ui")]
    pub struct SqweelWindow {
        #[template_child]
        pub nav: TemplateChild<adw::NavigationView>,
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
        let rx = crate::runtime::spawn(async move { driver.connect(&cfg).await });

        let this = self.clone();
        glib::spawn_future_local(async move {
            let result = rx.recv().await;
            this.set_busy(false);
            match result {
                Ok(Ok(conn)) => {
                    let conn: Arc<dyn Connection> = Arc::from(conn);
                    let page = MainView::new(conn, &cfg_for_view);
                    this.imp().nav.push(&page);
                }
                Ok(Err(e)) => this.toast(&e.to_string()),
                Err(_) => this.toast("Connection task was dropped"),
            }
        });
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
