use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};

use crate::db::{Connection, ConnectionConfig, DbError, Relation, RelationKind, ResultSet};
use crate::row_object::RowObject;
use crate::runtime;

/// Sidebar row metadata, aligned 1:1 with the rows in `relation_list`.
/// `None` marks a non-selectable section header.
type RowMeta = Option<(String, RelationKind, i64)>;

mod imp {
    use super::*;

    #[derive(Default, gtk::CompositeTemplate)]
    #[template(resource = "/com/marwa/sqweel/main_view.ui")]
    pub struct MainView {
        #[template_child]
        pub connection_name: TemplateChild<gtk::Label>,
        #[template_child]
        pub connection_host: TemplateChild<gtk::Label>,
        #[template_child]
        pub schema_dropdown: TemplateChild<gtk::DropDown>,
        #[template_child]
        pub search_entry: TemplateChild<gtk::SearchEntry>,
        #[template_child]
        pub relation_list: TemplateChild<gtk::ListBox>,
        #[template_child]
        pub breadcrumb: TemplateChild<gtk::Label>,
        #[template_child]
        pub row_summary: TemplateChild<gtk::Label>,
        #[template_child]
        pub column_view: TemplateChild<gtk::ColumnView>,
        #[template_child]
        pub status_left: TemplateChild<gtk::Label>,
        #[template_child]
        pub status_right: TemplateChild<gtk::Label>,

        pub conn: RefCell<Option<Arc<dyn Connection>>>,
        pub server_version: RefCell<String>,
        pub current_schema: RefCell<String>,
        pub current_table: RefCell<Option<String>>,
        pub row_meta: RefCell<Vec<RowMeta>>,
        pub search_text: RefCell<String>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for MainView {
        const NAME: &'static str = "MainView";
        type Type = super::MainView;
        type ParentType = adw::NavigationPage;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
            klass.bind_template_instance_callbacks();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for MainView {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();

            // Sidebar selection -> open table.
            self.relation_list.connect_row_selected(glib::clone!(
                #[weak]
                obj,
                move |_, row| {
                    if let Some(row) = row {
                        obj.on_relation_selected(row.index());
                    }
                }
            ));

            // Live search filter over the table list.
            self.search_entry.connect_search_changed(glib::clone!(
                #[weak]
                obj,
                move |entry| {
                    obj.imp()
                        .search_text
                        .replace(entry.text().to_lowercase());
                    obj.imp().relation_list.invalidate_filter();
                }
            ));

            self.relation_list.set_filter_func(glib::clone!(
                #[weak]
                obj,
                #[upgrade_or]
                true,
                move |row| obj.filter_row(row.index())
            ));

            // Schema dropdown -> reload relations.
            self.schema_dropdown.connect_selected_item_notify(glib::clone!(
                #[weak]
                obj,
                move |dd| {
                    if let Some(item) = dd.selected_item().and_downcast::<gtk::StringObject>() {
                        obj.load_schema(item.string().to_string());
                    }
                }
            ));
        }
    }

    impl WidgetImpl for MainView {}
    impl NavigationPageImpl for MainView {}
}

glib::wrapper! {
    pub struct MainView(ObjectSubclass<imp::MainView>)
        @extends adw::NavigationPage, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

#[gtk::template_callbacks]
impl MainView {
    pub fn new(conn: Arc<dyn Connection>, cfg: &ConnectionConfig) -> Self {
        let obj: Self = glib::Object::new();
        let imp = obj.imp();
        imp.conn.replace(Some(conn));
        imp.connection_name
            .set_text(&format!("{} · {}", cfg.database, cfg.host));
        imp.connection_host.set_text(&format!("{}:{}", cfg.host, cfg.port));
        obj.set_title(&cfg.database);
        obj.load_metadata();
        obj
    }

    fn conn(&self) -> Arc<dyn Connection> {
        self.imp().conn.borrow().as_ref().expect("connection set").clone()
    }

    /// Fetch server version + schema list, then load the first schema.
    fn load_metadata(&self) {
        let conn = self.conn();
        let started = std::time::Instant::now();
        let rx = runtime::spawn(async move {
            let version = conn.server_version().await?;
            let schemas = conn.schemas().await?;
            Ok::<_, DbError>((version, schemas))
        });

        let this = self.clone();
        glib::spawn_future_local(async move {
            match rx.recv().await {
                Ok(Ok((version, schemas))) => {
                    let ms = started.elapsed().as_millis();
                    this.imp().server_version.replace(version);
                    this.imp().status_right.set_text(&format!("connected · {ms} ms"));

                    let model = gtk::StringList::new(
                        &schemas.iter().map(String::as_str).collect::<Vec<_>>(),
                    );
                    this.imp().schema_dropdown.set_model(Some(&model));
                    // Prefer "public" if present.
                    if let Some(pos) = schemas.iter().position(|s| s == "public") {
                        this.imp().schema_dropdown.set_selected(pos as u32);
                    }
                    // selected_item_notify drives load_schema; fire once for
                    // the initial selection if it didn't change.
                    if let Some(item) = this
                        .imp()
                        .schema_dropdown
                        .selected_item()
                        .and_downcast::<gtk::StringObject>()
                    {
                        this.load_schema(item.string().to_string());
                    }
                }
                Ok(Err(e)) => this.imp().status_right.set_text(&format!("error: {e}")),
                Err(_) => {}
            }
        });
    }

    /// Load tables + views for `schema` into the sidebar.
    fn load_schema(&self, schema: String) {
        self.imp().current_schema.replace(schema.clone());
        let conn = self.conn();
        let schema_q = schema.clone();
        let rx = runtime::spawn(async move { conn.relations(&schema_q).await });

        let this = self.clone();
        glib::spawn_future_local(async move {
            match rx.recv().await {
                Ok(Ok(relations)) => this.populate_sidebar(relations),
                Ok(Err(e)) => this.imp().status_right.set_text(&format!("error: {e}")),
                Err(_) => {}
            }
        });
    }

    fn populate_sidebar(&self, relations: Vec<Relation>) {
        let imp = self.imp();
        let list = &imp.relation_list;

        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        let mut meta: Vec<RowMeta> = Vec::new();

        let tables: Vec<_> = relations
            .iter()
            .filter(|r| r.kind == RelationKind::Table)
            .collect();
        let views: Vec<_> = relations
            .iter()
            .filter(|r| r.kind == RelationKind::View)
            .collect();

        if !tables.is_empty() {
            list.append(&section_header("TABLES", tables.len()));
            meta.push(None);
            for r in &tables {
                list.append(&relation_row(&r.name, r.estimated_rows, "table-symbolic"));
                meta.push(Some((r.name.clone(), r.kind, r.estimated_rows)));
            }
        }
        if !views.is_empty() {
            list.append(&section_header("VIEWS", views.len()));
            meta.push(None);
            for r in &views {
                list.append(&relation_row(&r.name, r.estimated_rows, "view-reveal-symbolic"));
                meta.push(Some((r.name.clone(), r.kind, r.estimated_rows)));
            }
        }

        imp.row_meta.replace(meta);

        // Auto-open the first table.
        if let Some(idx) = imp.row_meta.borrow().iter().position(Option::is_some) {
            if let Some(row) = list.row_at_index(idx as i32) {
                list.select_row(Some(&row));
            }
        }
    }

    fn filter_row(&self, index: i32) -> bool {
        let meta = self.imp().row_meta.borrow();
        match meta.get(index as usize) {
            Some(Some((name, _, _))) => {
                let q = self.imp().search_text.borrow();
                q.is_empty() || name.to_lowercase().contains(q.as_str())
            }
            // Section headers always visible.
            _ => true,
        }
    }

    fn on_relation_selected(&self, index: i32) {
        let meta = self.imp().row_meta.borrow();
        if let Some(Some((name, _, est))) = meta.get(index as usize).cloned() {
            drop(meta);
            self.load_table(name, est);
        }
    }

    fn load_table(&self, table: String, estimated_rows: i64) {
        let schema = self.imp().current_schema.borrow().clone();
        self.imp().current_table.replace(Some(table.clone()));
        self.imp()
            .breadcrumb
            .set_text(&format!("{schema} › {table}"));

        let sql = format!(
            "SELECT * FROM \"{}\".\"{}\" LIMIT 1000",
            quote_ident(&schema),
            quote_ident(&table)
        );
        let conn = self.conn();
        let rx = runtime::spawn(async move { conn.query(&sql).await });

        let this = self.clone();
        glib::spawn_future_local(async move {
            match rx.recv().await {
                Ok(Ok(result)) => this.show_result(result, estimated_rows),
                Ok(Err(e)) => this.imp().row_summary.set_text(&e.to_string()),
                Err(_) => {}
            }
        });
    }

    fn show_result(&self, result: ResultSet, estimated_rows: i64) {
        self.rebuild_columns(&result);

        let store = gio::ListStore::new::<RowObject>();
        for row in &result.rows {
            store.append(&RowObject::new(Rc::new(row.values.clone())));
        }
        let selection = gtk::NoSelection::new(Some(store));
        self.imp().column_view.set_model(Some(&selection));

        let shown = result.rows.len();
        self.imp().row_summary.set_text(&format!(
            "{} rows · showing {shown}",
            group_thousands(estimated_rows)
        ));

        let imp = self.imp();
        let schema = imp.current_schema.borrow().clone();
        let table = imp.current_table.borrow().clone().unwrap_or_default();
        imp.status_left.set_text(&format!(
            "PostgreSQL {} · {schema} · {table} · {} rows",
            imp.server_version.borrow(),
            group_thousands(estimated_rows)
        ));
    }

    /// Rebuild the ColumnView's columns from the result set's schema.
    fn rebuild_columns(&self, result: &ResultSet) {
        let cv = &self.imp().column_view;
        let cols = cv.columns();
        while let Some(item) = cols.item(0) {
            let col = item.downcast::<gtk::ColumnViewColumn>().unwrap();
            cv.remove_column(&col);
        }

        // Leading row-number column.
        let num_factory = gtk::SignalListItemFactory::new();
        num_factory.connect_setup(|_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let label = gtk::Label::new(None);
            label.add_css_class("dim-label");
            label.set_xalign(1.0);
            item.set_child(Some(&label));
        });
        num_factory.connect_bind(|_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let label = item.child().and_downcast::<gtk::Label>().unwrap();
            label.set_text(&(item.position() + 1).to_string());
        });
        let num_col = gtk::ColumnViewColumn::new(Some("#"), Some(num_factory));
        cv.append_column(&num_col);

        for (idx, column) in result.columns.iter().enumerate() {
            let numeric = is_numeric(&column.data_type);
            let factory = gtk::SignalListItemFactory::new();
            factory.connect_setup(move |_, item| {
                let item = item.downcast_ref::<gtk::ListItem>().unwrap();
                let label = gtk::Label::new(None);
                label.set_xalign(if numeric { 1.0 } else { 0.0 });
                label.set_ellipsize(gtk::pango::EllipsizeMode::End);
                item.set_child(Some(&label));
            });
            factory.connect_bind(move |_, item| {
                let item = item.downcast_ref::<gtk::ListItem>().unwrap();
                let row = item.item().and_downcast::<RowObject>().unwrap();
                let label = item.child().and_downcast::<gtk::Label>().unwrap();
                label.set_text(&row.display(idx));
                if row.is_null(idx) {
                    label.add_css_class("dim-label");
                } else {
                    label.remove_css_class("dim-label");
                }
            });

            // Title carries the type as subtext via a newline.
            let title = format!("{}\n{}", column.name, column.data_type);
            let col = gtk::ColumnViewColumn::new(Some(&title), Some(factory));
            col.set_resizable(true);
            col.set_expand(false);
            cv.append_column(&col);
        }
    }

    #[template_callback]
    fn on_reload_clicked(&self) {
        let table = self.imp().current_table.borrow().clone();
        if let Some(table) = table {
            // Re-resolve the estimate from sidebar meta.
            let est = self
                .imp()
                .row_meta
                .borrow()
                .iter()
                .flatten()
                .find(|(n, _, _)| *n == table)
                .map(|(_, _, e)| *e)
                .unwrap_or(0);
            self.load_table(table, est);
        }
    }
}

fn section_header(title: &str, count: usize) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    row.set_selectable(false);
    row.set_activatable(false);

    let bx = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    bx.set_margin_top(8);
    bx.set_margin_start(4);

    let label = gtk::Label::new(Some(title));
    label.add_css_class("caption-heading");
    label.add_css_class("dim-label");
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_halign(gtk::Align::Start);

    let count = gtk::Label::new(Some(&count.to_string()));
    count.add_css_class("caption");
    count.add_css_class("dim-label");

    bx.append(&label);
    bx.append(&count);
    row.set_child(Some(&bx));
    row
}

fn relation_row(name: &str, rows: i64, icon: &str) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();

    let bx = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let image = gtk::Image::from_icon_name(icon);
    image.add_css_class("dim-label");

    let label = gtk::Label::new(Some(name));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_halign(gtk::Align::Start);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);

    let count = gtk::Label::new(Some(&group_thousands(rows)));
    count.add_css_class("caption");
    count.add_css_class("dim-label");

    bx.append(&image);
    bx.append(&label);
    bx.append(&count);
    row.set_child(Some(&bx));
    row
}

/// Double any embedded quotes so an identifier is safe inside `"..."`.
fn quote_ident(ident: &str) -> String {
    ident.replace('"', "\"\"")
}

fn is_numeric(data_type: &str) -> bool {
    matches!(
        data_type,
        "int2" | "int4" | "int8" | "numeric" | "float4" | "float8" | "money" | "oid"
    )
}

/// Format an integer with thousands separators ("12840" -> "12,840").
fn group_thousands(n: i64) -> String {
    let s = n.abs().to_string();
    let bytes = s.as_bytes();
    let mut out = String::new();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    if n < 0 {
        format!("-{out}")
    } else {
        out
    }
}
