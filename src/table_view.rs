use std::cell::RefCell;
use std::cmp::Ordering;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};

use crate::db::{Connection, ResultSet, Value};
use crate::row_object::RowObject;
use crate::runtime;

/// Horizontal cell padding (left + right) plus breathing room on top of the
/// measured text width.
const CELL_PADDING: i32 = 44;
const MIN_COL_WIDTH: i32 = 80;
/// Threshold a column width may not exceed; longer content ellipsizes.
const MAX_COL_WIDTH: i32 = 420;

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
        pub header_row: TemplateChild<gtk::Box>,
        #[template_child]
        pub header_scroll: TemplateChild<gtk::ScrolledWindow>,
        #[template_child]
        pub grid_scroll: TemplateChild<gtk::ScrolledWindow>,
        #[template_child]
        pub summary: TemplateChild<gtk::Label>,

        pub conn: RefCell<Option<Arc<dyn Connection>>>,
        pub schema: RefCell<String>,
        pub table: RefCell<String>,
        pub estimated_rows: std::cell::Cell<i64>,
        pub result: RefCell<Option<ResultSet>>,
        /// (column index, ascending) of the active client-side sort.
        pub sort: RefCell<Option<(usize, bool)>>,
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

    impl ObjectImpl for TableView {
        fn constructed(&self) {
            self.parent_constructed();
            // Scroll the header strip horizontally in lockstep with the grid.
            self.header_scroll
                .set_hadjustment(Some(&self.grid_scroll.hadjustment()));
        }
    }
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
                Ok(Ok(result)) => {
                    this.imp().sort.replace(None);
                    this.imp().result.replace(Some(result));
                    this.render();
                }
                Ok(Err(e)) => this.imp().summary.set_text(&e.to_string()),
                Err(_) => {}
            }
        });
    }

    /// Rebuild the custom header, the columns, and the row store from the
    /// stored result, honoring the current sort.
    fn render(&self) {
        let imp = self.imp();
        let guard = imp.result.borrow();
        let Some(result) = guard.as_ref() else { return };

        // Order the rows per the active sort.
        let mut rows: Vec<&crate::db::Row> = result.rows.iter().collect();
        if let Some((idx, asc)) = *imp.sort.borrow() {
            rows.sort_by(|a, b| {
                let o = cmp_values(a.values.get(idx), b.values.get(idx));
                if asc { o } else { o.reverse() }
            });
        }

        // Sample widths from a slice of the (unsorted) rows.
        let sample = &result.rows[..result.rows.len().min(60)];

        // Measure real text widths with Pango so columns fit their content,
        // capped at MAX_COL_WIDTH (longer values ellipsize).
        let pango = self.create_pango_context();
        let layout = gtk::pango::Layout::new(&pango);
        let measure = |s: &str| -> i32 {
            layout.set_text(s);
            layout.pixel_size().0
        };

        // Reset header + columns.
        while let Some(c) = imp.header_row.first_child() {
            imp.header_row.remove(&c);
        }
        let cv = &imp.column_view;
        let cols = cv.columns();
        while let Some(item) = cols.item(0) {
            cv.remove_column(&item.downcast::<gtk::ColumnViewColumn>().unwrap());
        }

        // Row-number column.
        let nwidth = ((result.rows.len().to_string().len().max(1) as i32) * 9 + 28).max(48);
        cv.append_column(&number_column(nwidth));
        imp.header_row.append(&number_header(nwidth));

        for (idx, column) in result.columns.iter().enumerate() {
            let numeric = is_numeric(&column.data_type);
            let badge = is_enumlike(&column.data_type);

            let mut content = measure(&column.name).max(measure(&column.data_type));
            for r in sample {
                if let Some(v) = r.values.get(idx) {
                    content = content.max(measure(&v.to_string()));
                }
            }
            // Padding (cell L/R) + dot for badges; cap at the threshold.
            let extra = CELL_PADDING + if badge { 18 } else { 0 };
            let width = (content + extra).clamp(MIN_COL_WIDTH, MAX_COL_WIDTH);

            // Hidden native header (title None); our header strip stands in.
            let col = gtk::ColumnViewColumn::new(None, Some(data_factory(idx, numeric, badge)));
            col.set_fixed_width(width);
            col.set_resizable(false);
            cv.append_column(&col);

            let active = imp.sort.borrow().filter(|(i, _)| *i == idx).map(|(_, a)| a);
            imp.header_row
                .append(&self.header_cell(idx, &column.name, &column.data_type, width, numeric, active));
        }

        // Hide ColumnView's native header (first child); our strip stands in.
        if let Some(header) = cv.first_child() {
            header.set_visible(false);
        }

        // Fill the store.
        let store = gio::ListStore::new::<RowObject>();
        for row in rows {
            store.append(&RowObject::new(Rc::new(row.values.clone())));
        }
        cv.set_model(Some(&gtk::NoSelection::new(Some(store))));

        let shown = result.rows.len();
        imp.summary.set_text(&format!(
            "{} rows · showing {shown}",
            group_thousands(self.estimated_rows())
        ));
    }

    /// A clickable two-line header cell (bold name + dim type) with a sort
    /// chevron when this column is the active sort.
    fn header_cell(
        &self,
        idx: usize,
        name: &str,
        dtype: &str,
        width: i32,
        numeric: bool,
        active: Option<bool>,
    ) -> gtk::Widget {
        let button = gtk::Button::new();
        button.add_css_class("flat");
        button.add_css_class("header-cell");
        button.set_size_request(width, -1);

        let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let align = if numeric { gtk::Align::End } else { gtk::Align::Start };
        vbox.set_halign(gtk::Align::Fill);

        let top = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        top.set_halign(align);
        let name_label = gtk::Label::new(Some(name));
        name_label.add_css_class("heading");
        name_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        top.append(&name_label);
        if let Some(asc) = active {
            let icon = if asc { "pan-up-symbolic" } else { "pan-down-symbolic" };
            let chevron = gtk::Image::from_icon_name(icon);
            chevron.add_css_class("accent");
            top.append(&chevron);
        }

        let type_label = gtk::Label::new(Some(dtype));
        type_label.add_css_class("dim-label");
        type_label.add_css_class("caption");
        type_label.set_halign(align);
        type_label.set_ellipsize(gtk::pango::EllipsizeMode::End);

        vbox.append(&top);
        vbox.append(&type_label);
        button.set_child(Some(&vbox));

        let this = self.clone();
        button.connect_clicked(move |_| this.sort_by(idx));
        button.upcast()
    }

    fn sort_by(&self, idx: usize) {
        let next = match *self.imp().sort.borrow() {
            Some((i, asc)) if i == idx => Some((idx, !asc)),
            _ => Some((idx, true)),
        };
        self.imp().sort.replace(next);
        self.render();
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

fn number_column(width: i32) -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let label = gtk::Label::new(None);
        label.add_css_class("dim-label");
        label.set_xalign(1.0);
        item.set_child(Some(&label));
    });
    factory.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let label = item.child().and_downcast::<gtk::Label>().unwrap();
        label.set_text(&(item.position() + 1).to_string());
    });
    let col = gtk::ColumnViewColumn::new(None, Some(factory));
    col.set_fixed_width(width);
    col.set_resizable(false);
    col
}

fn number_header(width: i32) -> gtk::Widget {
    let label = gtk::Label::new(Some("#"));
    label.add_css_class("dim-label");
    label.add_css_class("header-cell");
    label.set_xalign(1.0);
    label.set_size_request(width, -1);
    label.upcast()
}

fn data_factory(idx: usize, numeric: bool, badge: bool) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        item.set_child(Some(&cell_widget(badge, numeric)));
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
    factory
}

fn cell_widget(badge: bool, numeric: bool) -> gtk::Widget {
    if badge {
        let bx = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        bx.set_halign(gtk::Align::Start);
        bx.set_valign(gtk::Align::Center);
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

fn cmp_values(a: Option<&Value>, b: Option<&Value>) -> Ordering {
    use Value::*;
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, _) => Ordering::Greater,
        (_, None) => Ordering::Less,
        (Some(a), Some(b)) => match (a, b) {
            (Null, Null) => Ordering::Equal,
            (Null, _) => Ordering::Greater,
            (_, Null) => Ordering::Less,
            (Int(x), Int(y)) => x.cmp(y),
            (Float(x), Float(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
            (Int(x), Float(y)) => (*x as f64).partial_cmp(y).unwrap_or(Ordering::Equal),
            (Float(x), Int(y)) => x.partial_cmp(&(*y as f64)).unwrap_or(Ordering::Equal),
            (Bool(x), Bool(y)) => x.cmp(y),
            _ => a.to_string().cmp(&b.to_string()),
        },
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
