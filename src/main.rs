mod db;
mod runtime;
mod window;

use adw::prelude::*;
use gtk::{gio, glib};

use window::SqweelWindow;

const APP_ID: &str = "com.marwa.sqweel";

fn main() -> glib::ExitCode {
    gio::resources_register_include!("sqweel.gresource").expect("failed to register resources");

    let app = adw::Application::builder()
        .application_id(APP_ID)
        .build();

    app.connect_startup(|_| load_css());
    app.connect_activate(build_ui);

    app.run()
}

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_resource("/com/marwa/sqweel/style.css");
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

fn build_ui(app: &adw::Application) {
    let about = gio::ActionEntry::builder("about")
        .activate(|app: &adw::Application, _, _| {
            let about = adw::AboutWindow::builder()
                .application_name("sqweel")
                .application_icon(APP_ID)
                .developer_name("Marwa")
                .version(env!("CARGO_PKG_VERSION"))
                .comments("A database client and administration tool.")
                .build();
            if let Some(win) = app.active_window() {
                about.set_transient_for(Some(&win));
            }
            about.present();
        })
        .build();
    app.add_action_entries([about]);

    SqweelWindow::new(app).present();
}
