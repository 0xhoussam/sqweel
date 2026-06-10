//! A SQL scratch editor: a GtkSourceView input over a results pane. Statements
//! are routed by their leading keyword — queries (`SELECT`, `WITH`, …) render
//! in a `ResultGrid`; everything else runs as `execute` and reports rows
//! affected. Read-only: no inline editing, sorting, or pagination.

use std::cell::RefCell;
use std::sync::Arc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use sourceview5::prelude::*;

use crate::db::Connection;
use crate::result_grid::{GridOpts, ResultGrid};
use crate::runtime;

mod imp {
    use super::*;

    #[derive(Default, gtk::CompositeTemplate)]
    #[template(resource = "/com/marwa/sqweel/sql_view.ui")]
    pub struct SqlView {
        #[template_child]
        pub run_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub status: TemplateChild<gtk::Label>,
        #[template_child]
        pub editor_scroll: TemplateChild<gtk::ScrolledWindow>,
        #[template_child]
        pub result_stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub result_grid: TemplateChild<ResultGrid>,
        #[template_child]
        pub message_page: TemplateChild<adw::StatusPage>,
        #[template_child]
        pub error_label: TemplateChild<gtk::Label>,

        pub conn: RefCell<Option<Arc<dyn Connection>>>,
        pub schema: RefCell<String>,
        pub buffer: RefCell<Option<sourceview5::Buffer>>,
        /// A statement is in flight.
        pub running: std::cell::Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SqlView {
        const NAME: &'static str = "SqlView";
        type Type = super::SqlView;
        type ParentType = gtk::Box;

        fn class_init(klass: &mut Self::Class) {
            // Register the custom grid type so the template can instantiate it.
            ResultGrid::ensure_type();
            klass.bind_template();
            klass.bind_template_instance_callbacks();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for SqlView {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();

            // Build the SQL source editor and drop it into the scroller.
            let buffer = sourceview5::Buffer::new(None);
            if let Some(lang) = sourceview5::LanguageManager::default().language("sql") {
                buffer.set_language(Some(&lang));
            }
            buffer.set_highlight_syntax(true);
            let schemes = sourceview5::StyleSchemeManager::default();
            if let Some(scheme) = schemes.scheme("Adwaita").or_else(|| schemes.scheme("classic")) {
                buffer.set_style_scheme(Some(&scheme));
            }

            let view = sourceview5::View::with_buffer(&buffer);
            view.set_monospace(true);
            view.set_show_line_numbers(true);
            view.set_highlight_current_line(true);
            view.set_auto_indent(true);
            view.set_tab_width(2);
            view.set_insert_spaces_instead_of_tabs(true);
            view.set_top_margin(6);
            view.set_bottom_margin(6);
            view.set_left_margin(6);
            view.set_right_margin(6);
            self.editor_scroll.set_child(Some(&view));
            self.buffer.replace(Some(buffer));

            // Ctrl+Enter runs the current statement.
            let controller = gtk::ShortcutController::new();
            let trigger = gtk::ShortcutTrigger::parse_string("<Control>Return");
            let action = gtk::CallbackAction::new(glib::clone!(
                #[weak]
                obj,
                #[upgrade_or]
                glib::Propagation::Proceed,
                move |_, _| {
                    obj.run();
                    glib::Propagation::Stop
                }
            ));
            if let Some(trigger) = trigger {
                controller.add_shortcut(gtk::Shortcut::new(Some(trigger), Some(action)));
            }
            view.add_controller(controller);
        }
    }
    impl WidgetImpl for SqlView {}
    impl BoxImpl for SqlView {}
}

glib::wrapper! {
    pub struct SqlView(ObjectSubclass<imp::SqlView>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

#[gtk::template_callbacks]
impl SqlView {
    pub fn new(conn: Arc<dyn Connection>, schema: &str) -> Self {
        let obj: Self = glib::Object::new();
        let imp = obj.imp();
        imp.conn.replace(Some(conn));
        imp.schema.replace(schema.to_string());
        obj
    }

    pub fn schema(&self) -> String {
        self.imp().schema.borrow().clone()
    }

    fn conn(&self) -> Arc<dyn Connection> {
        self.imp().conn.borrow().as_ref().expect("connection set").clone()
    }

    /// Replace the editor contents (used by automated smoke tests).
    pub fn set_sql(&self, sql: &str) {
        if let Some(buffer) = self.imp().buffer.borrow().as_ref() {
            buffer.set_text(sql);
        }
    }

    /// Run the current statement (same as pressing Run / Ctrl+Enter).
    pub fn run_statement(&self) {
        self.run();
    }

    /// The full editor contents.
    fn sql_text(&self) -> String {
        let imp = self.imp();
        let guard = imp.buffer.borrow();
        let Some(buffer) = guard.as_ref() else { return String::new() };
        let (start, end) = buffer.bounds();
        buffer.text(&start, &end, false).to_string()
    }

    /// Run the editor's statement: queries fill the grid, other statements
    /// report rows affected. No-ops while a statement is already in flight.
    fn run(&self) {
        let imp = self.imp();
        if imp.running.get() {
            return;
        }
        let sql = self.sql_text();
        if sql.trim().is_empty() {
            return;
        }
        imp.running.set(true);
        imp.run_button.set_sensitive(false);
        imp.status.set_text("running…");

        let started = std::time::Instant::now();
        let conn = self.conn();

        if is_query(&sql) {
            let rx = runtime::spawn(async move { conn.query(&sql).await });
            let this = self.clone();
            glib::spawn_future_local(async move {
                let imp = this.imp();
                imp.running.set(false);
                imp.run_button.set_sensitive(true);
                let ms = started.elapsed().as_millis();
                match rx.recv().await {
                    Ok(Ok(result)) => {
                        let n = result.rows.len();
                        imp.result_grid.set_result(&result, GridOpts::default());
                        imp.result_stack.set_visible_child_name("grid");
                        imp.status.set_text(&format!(
                            "{n} row{} · {ms} ms",
                            if n == 1 { "" } else { "s" }
                        ));
                    }
                    Ok(Err(e)) => this.show_error(&e.to_string()),
                    Err(_) => imp.status.set_text(""),
                }
            });
        } else {
            let rx = runtime::spawn(async move { conn.execute(&sql).await });
            let this = self.clone();
            glib::spawn_future_local(async move {
                let imp = this.imp();
                imp.running.set(false);
                imp.run_button.set_sensitive(true);
                let ms = started.elapsed().as_millis();
                match rx.recv().await {
                    Ok(Ok(affected)) => {
                        imp.message_page.set_title(&format!(
                            "{affected} row{} affected",
                            if affected == 1 { "" } else { "s" }
                        ));
                        imp.result_stack.set_visible_child_name("message");
                        imp.status.set_text(&format!("done · {ms} ms"));
                    }
                    Ok(Err(e)) => this.show_error(&e.to_string()),
                    Err(_) => imp.status.set_text(""),
                }
            });
        }
    }

    fn show_error(&self, msg: &str) {
        let imp = self.imp();
        imp.error_label.set_text(msg);
        imp.result_stack.set_visible_child_name("error");
        imp.status.set_text("error");
    }

    #[template_callback]
    fn on_run_clicked(&self) {
        self.run();
    }
}

/// Whether a statement returns rows (and should render in the grid). Skips
/// leading whitespace and SQL comments, then inspects the first keyword.
fn is_query(sql: &str) -> bool {
    let mut s = sql.trim_start();
    loop {
        if let Some(rest) = s.strip_prefix("--") {
            s = match rest.find('\n') {
                Some(i) => rest[i + 1..].trim_start(),
                None => "",
            };
        } else if let Some(rest) = s.strip_prefix("/*") {
            s = match rest.find("*/") {
                Some(i) => rest[i + 2..].trim_start(),
                None => "",
            };
        } else {
            break;
        }
    }
    let kw: String = s.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    matches!(
        kw.to_ascii_uppercase().as_str(),
        "SELECT" | "WITH" | "SHOW" | "EXPLAIN" | "VALUES" | "TABLE"
    )
}

#[cfg(test)]
mod tests {
    use super::is_query;

    #[test]
    fn detects_select() {
        assert!(is_query("SELECT 1"));
        assert!(is_query("  select * from t"));
        assert!(is_query("WITH x AS (SELECT 1) SELECT * FROM x"));
    }

    #[test]
    fn detects_non_query() {
        assert!(!is_query("INSERT INTO t VALUES (1)"));
        assert!(!is_query("update t set a = 1"));
        assert!(!is_query("CREATE TABLE t (id int)"));
    }

    #[test]
    fn skips_comments() {
        assert!(is_query("-- a comment\nSELECT 1"));
        assert!(is_query("/* block */ select 1"));
        assert!(!is_query("-- note\ndelete from t"));
    }
}
