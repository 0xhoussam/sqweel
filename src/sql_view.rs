//! A minimal notebook-style SQL editor: a vertical list of cells. Each cell is
//! an independent GtkSourceView statement with its own Run / Delete buttons and
//! its own inline result (a read-only grid for queries, a message for other
//! statements, an error panel on failure). Statements route by their leading
//! keyword (see [`is_query`]). No ordering, no shared state between cells —
//! just the cell idea, kept deliberately small.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use lsp_types::Diagnostic;
use sourceview5::prelude::*;

use crate::completion::LspCompletionProvider;
use crate::db::Connection;
use crate::lsp::{self, LspClient};
use crate::result_grid::{GridOpts, ResultGrid};
use crate::runtime;

// ---------------------------------------------------------------------------
// SqlView — the cell container
// ---------------------------------------------------------------------------

mod imp {
    use super::*;

    #[derive(Default, gtk::CompositeTemplate)]
    #[template(resource = "/com/marwa/sqweel/sql_view.ui")]
    pub struct SqlView {
        #[template_child]
        pub cells_box: TemplateChild<gtk::Box>,

        pub conn: RefCell<Option<Arc<dyn Connection>>>,
        pub schema: RefCell<String>,
        pub cells: RefCell<Vec<super::SqlCell>>,
        pub lsp: RefCell<Option<LspClient>>,
        pub next_doc_id: Cell<u64>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SqlView {
        const NAME: &'static str = "SqlView";
        type Type = super::SqlView;
        type ParentType = gtk::Box;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
            klass.bind_template_instance_callbacks();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for SqlView {}
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
    pub fn new(conn: Arc<dyn Connection>, schema: &str, lsp: Option<LspClient>) -> Self {
        let obj: Self = glib::Object::new();
        let imp = obj.imp();
        imp.conn.replace(Some(conn));
        imp.schema.replace(schema.to_string());
        imp.lsp.replace(lsp);
        obj.add_cell();
        obj
    }

    pub fn schema(&self) -> String {
        self.imp().schema.borrow().clone()
    }

    fn conn(&self) -> Arc<dyn Connection> {
        self.imp().conn.borrow().as_ref().expect("connection set").clone()
    }

    /// Append a new empty cell and focus its editor; returns it.
    pub fn add_cell(&self) -> SqlCell {
        let imp = self.imp();
        let doc_id = imp.next_doc_id.get();
        imp.next_doc_id.set(doc_id + 1);
        let cell = SqlCell::new(self.conn(), &self.schema(), imp.lsp.borrow().clone(), doc_id);
        imp.cells_box.append(&cell);

        // Wire the cell's Delete button to remove it from the notebook.
        let weak = self.downgrade();
        let cell_weak = cell.downgrade();
        cell.set_on_delete(move || {
            if let (Some(this), Some(cell)) = (weak.upgrade(), cell_weak.upgrade()) {
                cell.close_lsp();
                this.imp().cells_box.remove(&cell);
                this.imp().cells.borrow_mut().retain(|c| c != &cell);
                // Keep at least one cell around to type into.
                if this.imp().cells.borrow().is_empty() {
                    this.add_cell();
                }
            }
        });

        imp.cells.borrow_mut().push(cell.clone());
        cell.focus_editor();
        cell
    }

    /// The first cell, if any (used by smoke tests).
    pub fn first_cell(&self) -> Option<SqlCell> {
        self.imp().cells.borrow().first().cloned()
    }

    /// Route diagnostics for `uri` to the matching cell.
    pub(crate) fn apply_diagnostics(&self, uri: &str, diags: &[Diagnostic]) {
        for cell in self.imp().cells.borrow().iter() {
            if cell.uri() == uri {
                cell.apply_diagnostics(diags);
                break;
            }
        }
    }

    #[template_callback]
    fn on_add_cell(&self) {
        self.add_cell();
    }
}

// ---------------------------------------------------------------------------
// SqlCell — one editor + result
// ---------------------------------------------------------------------------

mod cell_imp {
    use super::*;

    #[derive(Default)]
    pub struct SqlCell {
        pub conn: RefCell<Option<Arc<dyn Connection>>>,
        pub schema: RefCell<String>,
        pub view: RefCell<Option<sourceview5::View>>,
        pub buffer: RefCell<Option<sourceview5::Buffer>>,
        pub run_button: RefCell<Option<gtk::Button>>,
        pub status: RefCell<Option<gtk::Label>>,
        pub result_box: RefCell<Option<gtk::Box>>,
        pub result_stack: RefCell<Option<gtk::Stack>>,
        pub result_grid: RefCell<Option<ResultGrid>>,
        pub message_label: RefCell<Option<gtk::Label>>,
        pub error_label: RefCell<Option<gtk::Label>>,
        pub on_delete: RefCell<Option<Rc<dyn Fn()>>>,
        pub running: Cell<bool>,
        // LSP wiring (None when no language server is available).
        pub lsp: RefCell<Option<LspClient>>,
        pub uri: RefCell<String>,
        pub version: RefCell<Option<Arc<AtomicI64>>>,
        pub change_source: RefCell<Option<glib::SourceId>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SqlCell {
        const NAME: &'static str = "SqlCell";
        type Type = super::SqlCell;
        type ParentType = gtk::Box;
    }

    impl ObjectImpl for SqlCell {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_orientation(gtk::Orientation::Vertical);
            obj.add_css_class("sql-cell");

            // --- Toolbar: Run · status · Delete --------------------------------
            let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            toolbar.add_css_class("cell-toolbar");

            let run_button = gtk::Button::builder()
                .tooltip_text("Run (Ctrl+Enter)")
                .css_classes(["suggested-action"])
                .child(
                    &adw::ButtonContent::builder()
                        .icon_name("media-playback-start-symbolic")
                        .label("Run")
                        .build(),
                )
                .build();
            run_button.connect_clicked(glib::clone!(
                #[weak]
                obj,
                move |_| obj.run()
            ));

            let status = gtk::Label::builder()
                .css_classes(["dim-label", "caption"])
                .hexpand(true)
                .halign(gtk::Align::End)
                .build();

            let delete_button = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text("Delete cell")
                .css_classes(["flat"])
                .build();
            delete_button.connect_clicked(glib::clone!(
                #[weak]
                obj,
                move |_| {
                    if let Some(cb) = obj.imp().on_delete.borrow().as_ref() {
                        cb();
                    }
                }
            ));

            toolbar.append(&run_button);
            toolbar.append(&status);
            toolbar.append(&delete_button);

            // --- Editor --------------------------------------------------------
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

            let editor_scroll = gtk::ScrolledWindow::builder()
                .css_classes(["sql-editor"])
                .min_content_height(72)
                .max_content_height(240)
                .propagate_natural_height(true)
                .child(&view)
                .build();

            // Ctrl+Enter runs this cell.
            let controller = gtk::ShortcutController::new();
            if let Some(trigger) = gtk::ShortcutTrigger::parse_string("<Control>Return") {
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
                controller.add_shortcut(gtk::Shortcut::new(Some(trigger), Some(action)));
            }
            view.add_controller(controller);

            // --- Result (hidden until the first run) ---------------------------
            let result_grid = ResultGrid::new();
            result_grid.set_vexpand(false);
            result_grid.set_size_request(-1, 220);

            let message_label = gtk::Label::builder()
                .css_classes(["dim-label"])
                .xalign(0.0)
                .margin_top(8)
                .margin_bottom(8)
                .margin_start(8)
                .margin_end(8)
                .build();

            let error_label = gtk::Label::builder()
                .css_classes(["error", "monospace"])
                .xalign(0.0)
                .wrap(true)
                .selectable(true)
                .margin_top(8)
                .margin_bottom(8)
                .margin_start(8)
                .margin_end(8)
                .build();

            let result_stack = gtk::Stack::new();
            result_stack.add_named(&result_grid, Some("grid"));
            result_stack.add_named(&message_label, Some("message"));
            result_stack.add_named(&error_label, Some("error"));

            let result_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
            result_box.add_css_class("cell-result");
            result_box.append(&result_stack);
            result_box.set_visible(false);

            obj.append(&toolbar);
            obj.append(&editor_scroll);
            obj.append(&result_box);

            // Stash widgets we need later.
            self.view.replace(Some(view));
            self.buffer.replace(Some(buffer));
            self.run_button.replace(Some(run_button));
            self.status.replace(Some(status));
            self.result_box.replace(Some(result_box));
            self.result_stack.replace(Some(result_stack));
            self.result_grid.replace(Some(result_grid));
            self.message_label.replace(Some(message_label));
            self.error_label.replace(Some(error_label));
        }
    }
    impl WidgetImpl for SqlCell {}
    impl BoxImpl for SqlCell {}
}

glib::wrapper! {
    pub struct SqlCell(ObjectSubclass<cell_imp::SqlCell>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

impl SqlCell {
    fn new(
        conn: Arc<dyn Connection>,
        schema: &str,
        lsp: Option<LspClient>,
        doc_id: u64,
    ) -> Self {
        let obj: Self = glib::Object::new();
        let imp = obj.imp();
        imp.conn.replace(Some(conn));
        imp.schema.replace(schema.to_string());
        obj.setup_lsp(lsp, doc_id);
        obj
    }

    /// Open this cell's document with the language server and attach the
    /// completion provider. No-op when there's no server.
    fn setup_lsp(&self, lsp: Option<LspClient>, doc_id: u64) {
        let Some(client) = lsp else { return };
        let imp = self.imp();
        let uri = lsp::cell_uri(doc_id);
        client.did_open(&uri, "");
        let version = Arc::new(AtomicI64::new(1));

        if let (Some(view), Some(buffer)) =
            (imp.view.borrow().as_ref(), imp.buffer.borrow().as_ref())
        {
            let provider =
                LspCompletionProvider::new(client.clone(), &uri, buffer, version.clone());
            view.completion().add_provider(&provider);

            // Keep the server's copy in sync (debounced) so completion and
            // diagnostics reflect the latest text.
            let this = self.clone();
            buffer.connect_changed(move |_| this.schedule_did_change());
        }

        imp.lsp.replace(Some(client));
        imp.uri.replace(uri);
        imp.version.replace(Some(version));
    }

    /// Debounce `didChange` notifications while typing.
    fn schedule_did_change(&self) {
        let imp = self.imp();
        if let Some(id) = imp.change_source.take() {
            id.remove();
        }
        let this = self.clone();
        let id = glib::timeout_add_local_once(std::time::Duration::from_millis(200), move || {
            this.imp().change_source.take();
            this.send_did_change();
        });
        imp.change_source.replace(Some(id));
    }

    fn send_did_change(&self) {
        let imp = self.imp();
        let (Some(client), Some(version)) =
            (imp.lsp.borrow().clone(), imp.version.borrow().clone())
        else {
            return;
        };
        let v = version.fetch_add(1, Ordering::SeqCst) + 1;
        client.did_change(&imp.uri.borrow(), v, &self.sql_text());
    }

    /// This cell's LSP document URI ("" when no server).
    pub(crate) fn uri(&self) -> String {
        self.imp().uri.borrow().clone()
    }

    /// Underline diagnostic ranges and surface the first message in the status.
    pub(crate) fn apply_diagnostics(&self, diags: &[Diagnostic]) {
        let Some(buffer) = self.imp().buffer.borrow().clone() else { return };

        let table = buffer.tag_table();
        let tag = table.lookup("lsp-error").unwrap_or_else(|| {
            buffer
                .create_tag(Some("lsp-error"), &[("underline", &gtk::pango::Underline::Error)])
                .expect("create lsp-error tag")
        });

        let (start, end) = buffer.bounds();
        buffer.remove_tag(&tag, &start, &end);

        for d in diags {
            let a = buffer.iter_at_line_offset(d.range.start.line as i32, d.range.start.character as i32);
            let b = buffer.iter_at_line_offset(d.range.end.line as i32, d.range.end.character as i32);
            if let (Some(a), Some(b)) = (a, b) {
                buffer.apply_tag(&tag, &a, &b);
            }
        }

        match diags.first() {
            Some(first) => {
                self.set_status(&format!("⚠ {}", first.message.lines().next().unwrap_or("")))
            }
            None => self.set_status(""),
        }
    }

    /// Tell the server the document is gone (called before the cell is removed).
    pub(crate) fn close_lsp(&self) {
        let imp = self.imp();
        if let Some(client) = imp.lsp.borrow().clone() {
            let uri = imp.uri.borrow().clone();
            if !uri.is_empty() {
                client.did_close(&uri);
            }
        }
    }

    fn conn(&self) -> Arc<dyn Connection> {
        self.imp().conn.borrow().as_ref().expect("connection set").clone()
    }

    /// Register the handler invoked when the cell's Delete button is clicked.
    fn set_on_delete(&self, f: impl Fn() + 'static) {
        self.imp().on_delete.replace(Some(Rc::new(f)));
    }

    fn focus_editor(&self) {
        if let Some(view) = self.imp().view.borrow().as_ref() {
            view.grab_focus();
        }
    }

    /// Replace the cell's editor contents (used by smoke tests).
    pub fn set_sql(&self, sql: &str) {
        if let Some(buffer) = self.imp().buffer.borrow().as_ref() {
            buffer.set_text(sql);
        }
    }

    fn sql_text(&self) -> String {
        let imp = self.imp();
        let guard = imp.buffer.borrow();
        let Some(buffer) = guard.as_ref() else { return String::new() };
        let (start, end) = buffer.bounds();
        buffer.text(&start, &end, false).to_string()
    }

    fn set_status(&self, text: &str) {
        if let Some(label) = self.imp().status.borrow().as_ref() {
            label.set_text(text);
        }
    }

    fn show_result(&self, page: &str) {
        let imp = self.imp();
        if let Some(stack) = imp.result_stack.borrow().as_ref() {
            stack.set_visible_child_name(page);
        }
        if let Some(bx) = imp.result_box.borrow().as_ref() {
            bx.set_visible(true);
        }
    }

    /// Run this cell's statement: queries fill the grid, other statements
    /// report rows affected. No-ops while a statement is already in flight.
    pub fn run(&self) {
        let imp = self.imp();
        if imp.running.get() {
            return;
        }
        let sql = self.sql_text();
        if sql.trim().is_empty() {
            return;
        }
        imp.running.set(true);
        if let Some(btn) = imp.run_button.borrow().as_ref() {
            btn.set_sensitive(false);
        }
        self.set_status("running…");

        let started = std::time::Instant::now();
        let conn = self.conn();
        let query = is_query(&sql);

        let this = self.clone();
        if query {
            let rx = runtime::spawn(async move { conn.query(&sql).await });
            glib::spawn_future_local(async move {
                this.finish_running();
                let ms = started.elapsed().as_millis();
                match rx.recv().await {
                    Ok(Ok(result)) => {
                        let n = result.rows.len();
                        if let Some(grid) = this.imp().result_grid.borrow().as_ref() {
                            grid.set_result(&result, GridOpts::default());
                        }
                        this.show_result("grid");
                        this.set_status(&format!("{n} row{} · {ms} ms", plural(n)));
                    }
                    Ok(Err(e)) => this.show_error(&e.to_string()),
                    Err(_) => this.set_status(""),
                }
            });
        } else {
            let rx = runtime::spawn(async move { conn.execute(&sql).await });
            glib::spawn_future_local(async move {
                this.finish_running();
                let ms = started.elapsed().as_millis();
                match rx.recv().await {
                    Ok(Ok(affected)) => {
                        if let Some(label) = this.imp().message_label.borrow().as_ref() {
                            label.set_text(&format!(
                                "{affected} row{} affected",
                                plural(affected as usize)
                            ));
                        }
                        this.show_result("message");
                        this.set_status(&format!("done · {ms} ms"));
                    }
                    Ok(Err(e)) => this.show_error(&e.to_string()),
                    Err(_) => this.set_status(""),
                }
            });
        }
    }

    fn finish_running(&self) {
        let imp = self.imp();
        imp.running.set(false);
        if let Some(btn) = imp.run_button.borrow().as_ref() {
            btn.set_sensitive(true);
        }
    }

    fn show_error(&self, msg: &str) {
        if let Some(label) = self.imp().error_label.borrow().as_ref() {
            label.set_text(msg);
        }
        self.show_result("error");
        self.set_status("error");
    }
}

/// "" for 1, "s" otherwise.
fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
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
