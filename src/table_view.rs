use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};

use crate::db::{Connection, ResultSet};
use crate::row_object::RowObject;
use crate::runtime;

mod imp {
    use super::*;

    #[derive(Default, gtk::CompositeTemplate)]
    #[template(resource = "/com/marwa/sqweel/table_view.ui")]
    pub struct TableView {
        #[template_child]
        pub data_toggle: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub structure_toggle: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub indexes_toggle: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub view_stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub column_view: TemplateChild<gtk::ColumnView>,
        #[template_child]
        pub summary: TemplateChild<gtk::Label>,

        pub conn: RefCell<Option<Arc<dyn Connection>>>,
        pub schema: RefCell<String>,
        pub table: RefCell<String>,
        pub estimated_rows: Cell<i64>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TableView {
        const NAME: &'static str = "TableView";
        type Type = super::TableView;
        type ParentType = gtk::Box;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
            klass.bind_template_instance_callbacks();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for TableView {}
    impl WidgetImpl for TableView {}
    impl BoxImpl for TableView {}
}

glib::wrapper! {
    pub struct TableView(ObjectSubclass<imp::TableView>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

#[gtk::template_callbacks]
impl TableView {
    pub fn new(conn: Arc<dyn Connection>, schema: &str, table: &str, estimated_rows: i64) -> Self {
        let obj: Self = glib::Object::new();
        let imp = obj.imp();
        imp.conn.replace(Some(conn));
        imp.schema.replace(schema.to_string());
        imp.table.replace(table.to_string());
        imp.estimated_rows.set(estimated_rows);
        obj.load();
        obj
    }

    pub fn schema(&self) -> String {
        self.imp().schema.borrow().clone()
    }

    pub fn table(&self) -> String {
        self.imp().table.borrow().clone()
    }

    pub fn estimated_rows(&self) -> i64 {
        self.imp().estimated_rows.get()
    }

    fn conn(&self) -> Arc<dyn Connection> {
        self.imp().conn.borrow().as_ref().expect("connection set").clone()
    }

    pub fn reload(&self) {
        self.load();
    }

    fn load(&self) {
        let schema = self.schema();
        let table = self.table();
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
                Ok(Ok(result)) => this.show_result(result),
                Ok(Err(e)) => this.imp().summary.set_text(&e.to_string()),
                Err(_) => {}
            }
        });
    }

    fn show_result(&self, result: ResultSet) {
        self.rebuild_columns(&result);

        let store = gio::ListStore::new::<RowObject>();
        for row in &result.rows {
            store.append(&RowObject::new(Rc::new(row.values.clone())));
        }
        self.imp()
            .column_view
            .set_model(Some(&gtk::NoSelection::new(Some(store))));

        let shown = result.rows.len();
        self.imp().summary.set_text(&format!(
            "{} rows · showing {shown}",
            group_thousands(self.estimated_rows())
        ));
    }

    fn rebuild_columns(&self, result: &ResultSet) {
        let cv = &self.imp().column_view;
        let cols = cv.columns();
        while let Some(item) = cols.item(0) {
            cv.remove_column(&item.downcast::<gtk::ColumnViewColumn>().unwrap());
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
            let badge = is_enumlike(&column.data_type);

            let factory = gtk::SignalListItemFactory::new();
            factory.connect_setup(move |_, item| {
                let item = item.downcast_ref::<gtk::ListItem>().unwrap();
                let child = cell_widget(badge, numeric);
                item.set_child(Some(&child));
            });
            factory.connect_bind(move |_, item| {
                let item = item.downcast_ref::<gtk::ListItem>().unwrap();
                let row = item.item().and_downcast::<RowObject>().unwrap();
                let value = row.display(idx);
                if badge {
                    bind_badge(&item.child().unwrap(), &value, row.is_null(idx));
                } else {
                    let label = item.child().and_downcast::<gtk::Label>().unwrap();
                    label.set_text(&value);
                    if row.is_null(idx) {
                        label.add_css_class("dim-label");
                    } else {
                        label.remove_css_class("dim-label");
                    }
                }
            });

            let title = format!("{}\n{}", column.name, column.data_type);
            let col = gtk::ColumnViewColumn::new(Some(&title), Some(factory));
            col.set_resizable(true);
            cv.append_column(&col);
        }
    }

    #[template_callback]
    fn on_tab_toggled(&self) {
        let imp = self.imp();
        let name = if imp.structure_toggle.is_active() {
            "structure"
        } else if imp.indexes_toggle.is_active() {
            "indexes"
        } else {
            "data"
        };
        imp.view_stack.set_visible_child_name(name);
    }

    #[template_callback]
    fn on_reload_clicked(&self) {
        self.reload();
    }
}

/// Build the cell child: a badge box for enum-like columns, else a label.
fn cell_widget(badge: bool, numeric: bool) -> gtk::Widget {
    if badge {
        let bx = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        bx.set_halign(gtk::Align::Start);
        bx.add_css_class("badge");
        let dot = gtk::Label::new(Some("●"));
        dot.add_css_class("dot");
        let label = gtk::Label::new(None);
        bx.append(&dot);
        bx.append(&label);
        bx.upcast()
    } else {
        let label = gtk::Label::new(None);
        label.set_xalign(if numeric { 1.0 } else { 0.0 });
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.upcast()
    }
}

fn bind_badge(child: &gtk::Widget, value: &str, is_null: bool) {
    let bx = child.downcast_ref::<gtk::Box>().unwrap();
    let label = bx.last_child().and_downcast::<gtk::Label>().unwrap();
    label.set_text(value);
    for c in ["green", "yellow", "red", "neutral"] {
        bx.remove_css_class(c);
    }
    let variant = if is_null { "neutral" } else { badge_color(value) };
    bx.add_css_class(variant);
    bx.set_visible(!value.is_empty());
}

fn badge_color(value: &str) -> &'static str {
    match value.to_lowercase().as_str() {
        "paid" | "shipped" | "active" | "completed" | "done" | "success" | "enabled" | "true" => {
            "green"
        }
        "pending" | "processing" | "partial" | "warning" | "queued" => "yellow",
        "refunded" | "failed" | "cancelled" | "canceled" | "error" | "disabled" | "false" => "red",
        _ => "neutral",
    }
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

/// Builtin scalar types render as text; anything else (user-defined enums,
/// domains) gets a colored badge.
fn is_enumlike(data_type: &str) -> bool {
    !matches!(
        data_type,
        "int2"
            | "int4"
            | "int8"
            | "numeric"
            | "float4"
            | "float8"
            | "money"
            | "oid"
            | "bool"
            | "text"
            | "varchar"
            | "bpchar"
            | "char"
            | "name"
            | "uuid"
            | "json"
            | "jsonb"
            | "xml"
            | "bytea"
            | "date"
            | "time"
            | "timetz"
            | "timestamp"
            | "timestamptz"
            | "interval"
            | "inet"
            | "cidr"
            | "macaddr"
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
