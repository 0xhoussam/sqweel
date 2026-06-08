use std::cell::RefCell;
use std::sync::Arc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};

use crate::db::{Connection, ConnectionConfig, DbError, Relation, RelationKind};
use crate::runtime;
use crate::table_view::TableView;

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
        pub status_left: TemplateChild<gtk::Label>,
        #[template_child]
        pub status_right: TemplateChild<gtk::Label>,
        #[template_child]
        pub tab_view: TemplateChild<adw::TabView>,

        pub conn: RefCell<Option<Arc<dyn Connection>>>,
        pub server_version: RefCell<String>,
        pub current_schema: RefCell<String>,
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

            self.relation_list.connect_row_selected(glib::clone!(
                #[weak]
                obj,
                move |_, row| {
                    if let Some(row) = row {
                        obj.on_relation_selected(row.index());
                    }
                }
            ));

            self.search_entry.connect_search_changed(glib::clone!(
                #[weak]
                obj,
                move |entry| {
                    obj.imp().search_text.replace(entry.text().to_lowercase());
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

            self.schema_dropdown.connect_selected_item_notify(glib::clone!(
                #[weak]
                obj,
                move |dd| {
                    if let Some(item) = dd.selected_item().and_downcast::<gtk::StringObject>() {
                        obj.load_schema(item.string().to_string());
                    }
                }
            ));

            // Keep breadcrumb + status bar in sync with the active tab.
            self.tab_view.connect_selected_page_notify(glib::clone!(
                #[weak]
                obj,
                move |_| obj.update_chrome()
            ));

            // Honor tab close requests.
            self.tab_view.connect_close_page(|view, page| {
                view.close_page_finish(page, true);
                glib::Propagation::Stop
            });
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
                    this.imp()
                        .status_right
                        .set_text(&format!("● connected · {ms} ms"));

                    let model = gtk::StringList::new(
                        &schemas.iter().map(String::as_str).collect::<Vec<_>>(),
                    );
                    this.imp().schema_dropdown.set_model(Some(&model));
                    if let Some(pos) = schemas.iter().position(|s| s == "public") {
                        this.imp().schema_dropdown.set_selected(pos as u32);
                    }
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

        let tables: Vec<_> = relations.iter().filter(|r| r.kind == RelationKind::Table).collect();
        let views: Vec<_> = relations.iter().filter(|r| r.kind == RelationKind::View).collect();

        if !tables.is_empty() {
            list.append(&section_header("TABLES", tables.len()));
            meta.push(None);
            for r in &tables {
                list.append(&relation_row(&r.name, r.estimated_rows, "view-grid-symbolic"));
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
            _ => true,
        }
    }

    fn on_relation_selected(&self, index: i32) {
        let meta = self.imp().row_meta.borrow();
        if let Some(Some((name, _, est))) = meta.get(index as usize).cloned() {
            drop(meta);
            self.open_table(&name, est);
        }
    }

    /// Open a table in a tab, focusing an existing tab if already open.
    fn open_table(&self, table: &str, estimated_rows: i64) {
        let imp = self.imp();
        let schema = imp.current_schema.borrow().clone();

        // Focus an existing tab for this table.
        let pages = imp.tab_view.pages();
        for i in 0..pages.n_items() {
            let page = pages.item(i).and_downcast::<adw::TabPage>().unwrap();
            if let Some(tv) = page.child().downcast_ref::<TableView>() {
                if tv.schema() == schema && tv.table() == table {
                    imp.tab_view.set_selected_page(&page);
                    return;
                }
            }
        }

        let tv = TableView::new(self.conn(), &schema, table, estimated_rows);
        let page = imp.tab_view.append(&tv);
        page.set_title(table);
        page.set_icon(Some(&gio::ThemedIcon::new("view-grid-symbolic")));
        imp.tab_view.set_selected_page(&page);
    }

    fn update_chrome(&self) {
        let imp = self.imp();
        let Some(page) = imp.tab_view.selected_page() else {
            imp.breadcrumb.set_text("—");
            imp.status_left.set_text("");
            return;
        };
        let tv = page.child().downcast::<TableView>().unwrap();
        imp.breadcrumb
            .set_text(&format!("{} › {}", tv.schema(), tv.table()));
        imp.status_left.set_text(&format!(
            "PostgreSQL {} · {} · {} · {} rows",
            imp.server_version.borrow(),
            tv.schema(),
            tv.table(),
            group_thousands(tv.estimated_rows())
        ));
    }

    fn active_table(&self) -> Option<TableView> {
        self.imp()
            .tab_view
            .selected_page()
            .and_then(|p| p.child().downcast::<TableView>().ok())
    }

    #[template_callback]
    fn on_header_reload(&self) {
        if let Some(tv) = self.active_table() {
            tv.reload();
        }
    }

    #[template_callback]
    fn on_add_tab(&self) {
        // Re-open the currently selected sidebar relation (no-op if already open).
        if let Some(row) = self.imp().relation_list.selected_row() {
            self.on_relation_selected(row.index());
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
