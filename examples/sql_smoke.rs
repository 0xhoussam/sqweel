//! Headed smoke test for the SQL editor: connect, build MainView, open a SQL
//! editor tab, run a query, leave the window up briefly (for a screenshot),
//! then quit. Catches SqlView/ResultGrid template-binding panics that only
//! fire at runtime. Needs a display + the seeded throwaway DB.
//!
//!   cargo run --example sql_smoke
//!
//! Exits 0 on success; aborts on any panic.

use std::sync::Arc;

use adw::prelude::*;
use gtk::{gio, glib};

use sqweel::db::{self, Connection, ConnectionConfig};
use sqweel::main_view::MainView;

fn main() {
    gio::resources_register_include!("sqweel.gresource").expect("register resources");

    let app = adw::Application::builder()
        .application_id("com.marwa.sqweel.sqlsmoke")
        .build();

    app.connect_startup(|_| {
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceLight);
        let provider = gtk::CssProvider::new();
        provider.load_from_resource("/com/marwa/sqweel/style.css");
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });

    app.connect_activate(|app| {
        let cfg = ConnectionConfig {
            driver_id: "postgres".into(),
            host: "localhost".into(),
            port: 5432,
            database: "analytics".into(),
            username: "marwa".into(),
            password: "marwa".into(),
            ssl: false,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let conn = rt
            .block_on(async { db::driver("postgres").unwrap().connect(&cfg).await })
            .expect("connect to seeded postgres");
        let conn: Arc<dyn Connection> = Arc::from(conn);

        let view = MainView::new(conn, &cfg);
        let nav = adw::NavigationView::new();
        nav.push(&view);

        let win = adw::ApplicationWindow::builder()
            .application(app)
            .default_width(1100)
            .default_height(760)
            .build();
        win.set_content(Some(&nav));
        win.present();

        // Let the sidebar/first-table load, then open a SQL editor and run a
        // query against the seeded data.
        glib::timeout_add_seconds_local_once(
            1,
            glib::clone!(
                #[weak]
                view,
                move || {
                    let sql = view.open_sql_editor();
                    sql.set_sql(
                        "SELECT o.id, c.name, o.status, o.total, o.note\n\
                         FROM orders o JOIN customers c ON c.id = o.customer_id\n\
                         ORDER BY o.id",
                    );
                    sql.run_statement();
                }
            ),
        );

        // Keep the window up long enough to observe / screenshot, then quit.
        glib::timeout_add_seconds_local_once(
            15,
            glib::clone!(
                #[weak]
                app,
                move || {
                    println!("sql_smoke: no panic, quitting");
                    app.quit();
                }
            ),
        );
    });

    app.run();
}
