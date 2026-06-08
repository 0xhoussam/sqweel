//! Headed smoke test: connect to a live postgres, build the MainView, run the
//! loop briefly, and exit. Catches template-binding / signal panics that only
//! fire at runtime. Needs a display + the seeded throwaway DB:
//!
//!   cargo run --example gui_smoke
//!
//! Exits 0 on success; aborts (non-zero) on any panic.

use std::sync::Arc;

use adw::prelude::*;
use gtk::{gio, glib};

use sqweel::db::{self, Connection, ConnectionConfig};
use sqweel::main_view::MainView;

fn main() {
    gio::resources_register_include!("sqweel.gresource").expect("register resources");

    let app = adw::Application::builder()
        .application_id("com.marwa.sqweel.smoke")
        .build();

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
            .default_width(1000)
            .default_height(700)
            .build();
        win.set_content(Some(&nav));
        win.present();

        // Let async sidebar/grid loads run, then quit cleanly.
        glib::timeout_add_seconds_local_once(
            3,
            glib::clone!(
                #[weak]
                app,
                move || {
                    println!("gui_smoke: no panic, quitting");
                    app.quit();
                }
            ),
        );
    });

    app.run_with_args::<&str>(&[]);
}
