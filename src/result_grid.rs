//! A read-only result grid: custom two-line header strip over a `ColumnView`,
//! with Pango-measured column widths, numeric alignment, enum-like badges, and
//! a row-number column. Shared by `TableView` (which layers editing, sorting,
//! and pagination on top via `GridOpts`) and `SqlView` (read-only).

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gio, glib};

use crate::db::{ColumnInfo, ResultSet, Row};
use crate::row_object::RowObject;

/// Horizontal cell padding (left + right) plus breathing room on top of the
/// measured text width.
const CELL_PADDING: i32 = 44;
const MIN_COL_WIDTH: i32 = 80;
/// Threshold a column width may not exceed; longer content ellipsizes.
const MAX_COL_WIDTH: i32 = 420;

/// Header-click handler, given the clicked column index.
pub type SortFn = Rc<dyn Fn(usize)>;
/// Cell double-click handler: (row, column index, column name, cell widget).
pub type EditFn = Rc<dyn Fn(RowObject, usize, String, gtk::Widget)>;
/// Cell right-click handler: (row, column index, cell widget).
pub type ContextFn = Rc<dyn Fn(RowObject, usize, gtk::Widget)>;

/// Per-render behavior knobs. Defaults yield a fully read-only grid.
#[derive(Default)]
pub struct GridOpts {
    /// Column metadata for PK/FK header decorations (matched by name). May be empty.
    pub meta: Vec<ColumnInfo>,
    /// Active sort as (column index, ascending) — drives the header chevron.
    pub sort: Option<(usize, bool)>,
    /// Whether cells may be edited in place (requires `on_edit` and a PK).
    pub editable: bool,
    /// Header-click handler; `None` disables sorting (no click action).
    pub on_sort: Option<SortFn>,
    /// Cell double-click handler; `None` makes cells read-only.
    pub on_edit: Option<EditFn>,
    /// Cell right-click handler; `None` disables the context menu.
    pub on_context: Option<ContextFn>,
}

mod imp {
    use super::*;

    #[derive(Default, gtk::CompositeTemplate)]
    #[template(resource = "/com/marwa/sqweel/result_grid.ui")]
    pub struct ResultGrid {
        #[template_child]
        pub header_row: TemplateChild<gtk::Box>,
        #[template_child]
        pub header_scroll: TemplateChild<gtk::ScrolledWindow>,
        #[template_child]
        pub grid_scroll: TemplateChild<gtk::ScrolledWindow>,
        #[template_child]
        pub column_view: TemplateChild<gtk::ColumnView>,

        /// The live row store, kept so later pages can append.
        pub store: RefCell<Option<gio::ListStore>>,
        /// Fired when the grid is scrolled near the bottom (infinite scroll).
        pub on_near_bottom: RefCell<Option<Rc<dyn Fn()>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ResultGrid {
        const NAME: &'static str = "ResultGrid";
        type Type = super::ResultGrid;
        type ParentType = gtk::Box;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for ResultGrid {
        fn constructed(&self) {
            self.parent_constructed();
            // Scroll the header strip horizontally in lockstep with the grid.
            self.header_scroll
                .set_hadjustment(Some(&self.grid_scroll.hadjustment()));

            // Fire the near-bottom hook when scrolled close to the end.
            let obj = self.obj();
            self.grid_scroll.vadjustment().connect_value_changed(glib::clone!(
                #[weak]
                obj,
                move |adj| {
                    let near_bottom =
                        adj.value() + adj.page_size() >= adj.upper() - adj.page_size().max(200.0);
                    if near_bottom && let Some(cb) = obj.imp().on_near_bottom.borrow().as_ref() {
                        cb();
                    }
                }
            ));
        }
    }
    impl WidgetImpl for ResultGrid {}
    impl BoxImpl for ResultGrid {}
}

glib::wrapper! {
    pub struct ResultGrid(ObjectSubclass<imp::ResultGrid>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

impl Default for ResultGrid {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl ResultGrid {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a callback fired when the grid is scrolled near its bottom.
    pub fn set_on_near_bottom(&self, f: impl Fn() + 'static) {
        self.imp().on_near_bottom.replace(Some(Rc::new(f)));
    }

    /// Number of rows currently in the store.
    pub fn row_count(&self) -> u32 {
        self.imp().store.borrow().as_ref().map_or(0, |s| s.n_items())
    }

    /// The live row store (loaded rows), if a result has been set.
    pub fn store(&self) -> Option<gio::ListStore> {
        self.imp().store.borrow().clone()
    }

    /// Append a page of rows to the existing store (infinite scroll).
    pub fn append_rows(&self, rows: &[Row]) {
        if let Some(store) = self.imp().store.borrow().as_ref() {
            for row in rows {
                store.append(&RowObject::new(Rc::new(row.values.clone())));
            }
        }
    }

    /// Rebuild the header + columns and seed the store with `result`'s rows.
    pub fn set_result(&self, result: &ResultSet, opts: GridOpts) {
        let imp = self.imp();

        // Sample widths from a slice of the rows.
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

        let meta = &opts.meta;

        for (idx, column) in result.columns.iter().enumerate() {
            let numeric = is_numeric(&column.data_type);
            let badge = is_enumlike(&column.data_type);
            let info = meta.iter().find(|c| c.name == column.name);
            let is_pk = info.is_some_and(|c| c.is_primary_key);
            let is_fk = info.is_some_and(|c| c.is_foreign_key);

            let mut content = measure(&column.name).max(measure(&column.data_type));
            for r in sample {
                if let Some(v) = r.values.get(idx) {
                    content = content.max(measure(&v.to_string()));
                }
            }
            // Padding (cell L/R) + dot for badges + key/link icon; cap.
            let extra = CELL_PADDING + if badge { 18 } else { 0 } + if is_pk || is_fk { 18 } else { 0 };
            let width = (content + extra).clamp(MIN_COL_WIDTH, MAX_COL_WIDTH);

            // Edit non-badge cells in place when editing is enabled.
            let edit_cb = if opts.editable && !badge { opts.on_edit.clone() } else { None };
            let factory =
                data_factory(idx, &column.name, numeric, badge, is_pk, edit_cb, opts.on_context.clone());
            let col = gtk::ColumnViewColumn::new(None, Some(factory));
            col.set_fixed_width(width);
            col.set_resizable(false);
            cv.append_column(&col);

            let active = opts.sort.filter(|(i, _)| *i == idx).map(|(_, a)| a);
            imp.header_row.append(&header_cell(
                idx,
                &column.name,
                &column.data_type,
                width,
                numeric,
                active,
                is_pk,
                is_fk,
                opts.on_sort.clone(),
            ));
        }

        // Hide ColumnView's native header (first child); our strip stands in.
        if let Some(header) = cv.first_child() {
            header.set_visible(false);
        }

        // Seed the store with this result; later pages append to it.
        let store = gio::ListStore::new::<RowObject>();
        for row in &result.rows {
            store.append(&RowObject::new(Rc::new(row.values.clone())));
        }
        cv.set_model(Some(&gtk::NoSelection::new(Some(store.clone()))));
        imp.store.replace(Some(store));
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

/// Build a column's cell factory, wiring double-click editing when `on_edit`
/// is supplied.
fn data_factory(
    idx: usize,
    col_name: &str,
    numeric: bool,
    badge: bool,
    is_pk: bool,
    on_edit: Option<EditFn>,
    on_context: Option<ContextFn>,
) -> gtk::SignalListItemFactory {
    let factory = data_factory_inner(idx, numeric, badge, is_pk);
    if let Some(cb) = on_edit {
        let col = col_name.to_string();
        // Second setup pass: attach a double-click handler to the cell.
        factory.connect_setup(move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap().clone();
            let Some(child) = item.child() else { return };
            let gesture = gtk::GestureClick::new();
            let cb = cb.clone();
            let col = col.clone();
            let anchor = child.clone();
            gesture.connect_pressed(move |_, n_press, _, _| {
                if n_press != 2 {
                    return;
                }
                let Some(row) = item.item().and_downcast::<RowObject>() else {
                    return;
                };
                cb(row, idx, col.clone(), anchor.clone());
            });
            child.add_controller(gesture);
        });
    }
    if let Some(cb) = on_context {
        // Third setup pass: a secondary-click (right-click) handler.
        factory.connect_setup(move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap().clone();
            let Some(child) = item.child() else { return };
            let gesture = gtk::GestureClick::new();
            gesture.set_button(gtk::gdk::BUTTON_SECONDARY);
            let cb = cb.clone();
            let anchor = child.clone();
            gesture.connect_pressed(move |_, _, _, _| {
                let Some(row) = item.item().and_downcast::<RowObject>() else {
                    return;
                };
                cb(row, idx, anchor.clone());
            });
            child.add_controller(gesture);
        });
    }
    factory
}

/// A clickable two-line header cell (bold name + dim type) with a sort chevron
/// when this column is the active sort. Click fires `on_sort` if provided.
#[allow(clippy::too_many_arguments)]
fn header_cell(
    idx: usize,
    name: &str,
    dtype: &str,
    width: i32,
    numeric: bool,
    active: Option<bool>,
    is_pk: bool,
    is_fk: bool,
    on_sort: Option<SortFn>,
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
    if is_pk {
        let key = gtk::Image::from_icon_name("dialog-password-symbolic");
        key.add_css_class("accent");
        key.set_tooltip_text(Some("Primary key"));
        top.append(&key);
    } else if is_fk {
        let link = gtk::Image::from_icon_name("insert-link-symbolic");
        link.add_css_class("dim-label");
        link.set_tooltip_text(Some("Foreign key"));
        top.append(&link);
    }
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

    if let Some(cb) = on_sort {
        button.connect_clicked(move |_| cb(idx));
    } else {
        // Not sortable: keep the visual cell but make it inert.
        button.set_sensitive(false);
        button.set_can_focus(false);
    }
    button.upcast()
}

fn data_factory_inner(idx: usize, numeric: bool, badge: bool, is_pk: bool) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        item.set_child(Some(&cell_widget(badge, numeric, is_pk)));
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

fn cell_widget(badge: bool, numeric: bool, is_pk: bool) -> gtk::Widget {
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
        if is_pk {
            // Primary-key values read like links, matching the mockup.
            label.add_css_class("cell-link");
        }
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
