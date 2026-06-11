use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};

use crate::db::{ColumnInfo, Connection, IndexInfo, RelationKind, ResultSet, Value};
use crate::result_grid::{ContextFn, GridOpts, ResultGrid};
use crate::row_object::RowObject;
use crate::runtime;

/// Rows fetched per page (infinite scroll).
const PAGE_SIZE: usize = 500;

/// A comparison used by a column filter.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FilterOp {
    Eq,
    Ne,
    Lt,
    Gt,
    Contains,
    IsNull,
    IsNotNull,
}

impl FilterOp {
    /// All operators, in display order, paired with their dropdown label.
    const ALL: [(FilterOp, &'static str); 7] = [
        (FilterOp::Eq, "="),
        (FilterOp::Ne, "≠"),
        (FilterOp::Lt, "<"),
        (FilterOp::Gt, ">"),
        (FilterOp::Contains, "contains"),
        (FilterOp::IsNull, "is null"),
        (FilterOp::IsNotNull, "is not null"),
    ];

    /// Whether this operator needs a value (the others are unary).
    fn takes_value(self) -> bool {
        !matches!(self, FilterOp::IsNull | FilterOp::IsNotNull)
    }
}

/// A single column filter: `column op value`.
#[derive(Clone)]
pub struct Filter {
    pub column: String,
    pub op: FilterOp,
    pub value: String,
}

impl Filter {
    /// Render as a SQL boolean expression with the identifier quoted and the
    /// value escaped as a string literal (Postgres coerces it to the column's
    /// type).
    fn to_sql(&self) -> String {
        let col = format!("\"{}\"", quote_ident(&self.column));
        let lit = || format!("'{}'", self.value.replace('\'', "''"));
        match self.op {
            FilterOp::Eq => format!("{col} = {}", lit()),
            FilterOp::Ne => format!("{col} <> {}", lit()),
            FilterOp::Lt => format!("{col} < {}", lit()),
            FilterOp::Gt => format!("{col} > {}", lit()),
            FilterOp::Contains => {
                let pat = escape_like(&self.value).replace('\'', "''");
                format!("CAST({col} AS text) ILIKE '%{pat}%' ESCAPE '\\'")
            }
            FilterOp::IsNull => format!("{col} IS NULL"),
            FilterOp::IsNotNull => format!("{col} IS NOT NULL"),
        }
    }
}

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
        pub result_grid: TemplateChild<ResultGrid>,
        #[template_child]
        pub summary: TemplateChild<gtk::Label>,
        #[template_child]
        pub structure_group: TemplateChild<adw::PreferencesGroup>,
        #[template_child]
        pub indexes_group: TemplateChild<adw::PreferencesGroup>,
        #[template_child]
        pub search_bar: TemplateChild<gtk::SearchBar>,
        #[template_child]
        pub search_entry: TemplateChild<gtk::SearchEntry>,
        #[template_child]
        pub filter_button: TemplateChild<gtk::Button>,

        pub conn: RefCell<Option<Arc<dyn Connection>>>,
        pub schema: RefCell<String>,
        pub table: RefCell<String>,
        pub estimated_rows: std::cell::Cell<i64>,
        /// True for tables/partitioned tables (have a `ctid`); false for views.
        pub is_table: std::cell::Cell<bool>,
        pub result: RefCell<Option<ResultSet>>,
        pub columns: RefCell<Vec<ColumnInfo>>,
        pub structure_rows: RefCell<Vec<adw::ActionRow>>,
        pub indexes_rows: RefCell<Vec<adw::ActionRow>>,
        /// Primary-key columns as (result index, name) — empty disables editing.
        pub pk_cols: RefCell<Vec<(usize, String)>>,
        /// (column index, ascending) of the active client-side sort.
        pub sort: RefCell<Option<(usize, bool)>>,
        pub search_term: RefCell<String>,
        /// Pending debounced search timeout.
        pub search_source: RefCell<Option<glib::SourceId>>,
        /// Active column filters, AND-combined into the WHERE clause.
        pub filters: RefCell<Vec<Filter>>,
        /// Rows fetched so far (the OFFSET for the next page).
        pub offset: std::cell::Cell<usize>,
        /// A page fetch is in flight.
        pub loading: std::cell::Cell<bool>,
        /// Last page was short — no more rows to fetch.
        pub exhausted: std::cell::Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TableView {
        const NAME: &'static str = "TableView";
        type Type = super::TableView;
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

    impl ObjectImpl for TableView {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();

            // Load the next page when the grid is scrolled near the bottom.
            self.result_grid.set_on_near_bottom(glib::clone!(
                #[weak]
                obj,
                move || obj.load_more()
            ));

            // Ctrl+F / typing opens the search bar; Esc closes it.
            self.search_bar.set_key_capture_widget(Some(&*obj));

            // Closing the search bar clears the query and reloads all rows.
            self.search_bar.connect_search_mode_enabled_notify(glib::clone!(
                #[weak]
                obj,
                move |bar| {
                    if !bar.is_search_mode() && !obj.imp().search_term.borrow().is_empty() {
                        obj.imp().search_entry.set_text("");
                    }
                }
            ));
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
    pub fn new(
        conn: Arc<dyn Connection>,
        schema: &str,
        table: &str,
        kind: RelationKind,
        estimated_rows: i64,
    ) -> Self {
        let obj: Self = glib::Object::new();
        let imp = obj.imp();
        imp.conn.replace(Some(conn));
        imp.schema.replace(schema.to_string());
        imp.table.replace(table.to_string());
        imp.is_table.set(kind == RelationKind::Table);
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

    pub fn toggle_search(&self) {
        let bar = &self.imp().search_bar;
        let on = !bar.is_search_mode();
        bar.set_search_mode(on);
        if on {
            self.imp().search_entry.grab_focus();
        }
    }

    /// Full (re)load from the first page: refetch metadata + first page.
    fn load(&self) {
        let imp = self.imp();
        if imp.loading.get() {
            return;
        }
        imp.loading.set(true);
        imp.offset.set(0);
        imp.exhausted.set(false);

        let schema = self.schema();
        let table = self.table();
        let term = imp.search_term.borrow().clone();
        let names: Vec<String> = imp.columns.borrow().iter().map(|c| c.name.clone()).collect();
        let filters = imp.filters.borrow().clone();
        let order = self.order_target();
        let sql = build_query(&schema, &table, &term, &names, &filters, order.as_ref(), PAGE_SIZE, 0);
        let conn = self.conn();
        let (sc, tb) = (schema.clone(), table.clone());
        let rx = runtime::spawn(async move {
            let columns = conn.columns(&sc, &tb).await?;
            let indexes = conn.indexes(&sc, &tb).await?;
            let data = conn.query(&sql).await?;
            Ok::<_, crate::db::DbError>((columns, indexes, data))
        });

        let this = self.clone();
        glib::spawn_future_local(async move {
            let imp = this.imp();
            imp.loading.set(false);
            match rx.recv().await {
                Ok(Ok((columns, indexes, result))) => {
                    let n = result.rows.len();
                    imp.columns.replace(columns);
                    imp.result.replace(Some(result));
                    this.render();
                    this.build_structure();
                    this.build_indexes(indexes);
                    imp.offset.set(n);
                    imp.exhausted.set(n < PAGE_SIZE);
                }
                Ok(Err(e)) => this.imp().summary.set_text(&e.to_string()),
                Err(_) => {}
            }
        });
    }

    /// Fetch the next page and append it to the store (infinite scroll).
    fn load_more(&self) {
        let imp = self.imp();
        if imp.loading.get() || imp.exhausted.get() || imp.result.borrow().is_none() {
            return;
        }
        imp.loading.set(true);

        let schema = self.schema();
        let table = self.table();
        let term = imp.search_term.borrow().clone();
        let names: Vec<String> = imp.columns.borrow().iter().map(|c| c.name.clone()).collect();
        let filters = imp.filters.borrow().clone();
        let order = self.order_target();
        let offset = imp.offset.get();
        let sql = build_query(&schema, &table, &term, &names, &filters, order.as_ref(), PAGE_SIZE, offset);
        let conn = self.conn();
        let rx = runtime::spawn(async move { conn.query(&sql).await });

        let this = self.clone();
        glib::spawn_future_local(async move {
            let imp = this.imp();
            imp.loading.set(false);
            match rx.recv().await {
                Ok(Ok(result)) => {
                    let n = result.rows.len();
                    this.imp().result_grid.append_rows(&result.rows);
                    imp.offset.set(offset + n);
                    imp.exhausted.set(n < PAGE_SIZE);
                    this.update_summary();
                }
                Ok(Err(e)) => this.imp().summary.set_text(&e.to_string()),
                Err(_) => {}
            }
        });
    }

    /// The column the grid is ordered by: the active sort, else the primary key
    /// (for stable pagination), else none.
    fn order_target(&self) -> Option<(String, bool)> {
        let imp = self.imp();
        if let Some((idx, asc)) = *imp.sort.borrow() {
            if let Some(result) = imp.result.borrow().as_ref() {
                if let Some(col) = result.columns.get(idx) {
                    return Some((col.name.clone(), asc));
                }
            }
        }
        if let Some(pk) = imp
            .columns
            .borrow()
            .iter()
            .find(|c| c.is_primary_key)
        {
            return Some((pk.name.clone(), true));
        }
        // No PK: order by the physical row id so OFFSET paging is stable.
        // Only tables have a `ctid`; views can't be ordered this way.
        if imp.is_table.get() {
            return Some(("ctid".to_string(), true));
        }
        None
    }

    fn update_summary(&self) {
        let imp = self.imp();
        let shown = imp.result_grid.row_count();
        if imp.search_term.borrow().trim().is_empty() {
            imp.summary.set_text(&format!(
                "{} rows · showing {shown}",
                group_thousands(self.estimated_rows())
            ));
        } else {
            imp.summary
                .set_text(&format!("{shown} matching row{}", if shown == 1 { "" } else { "s" }));
        }
    }

    /// Rebuild the custom header + columns and seed the store with the first
    /// page. Ordering is done server-side (see `order_target`).
    fn render(&self) {
        let imp = self.imp();
        let guard = imp.result.borrow();
        let Some(result) = guard.as_ref() else { return };

        // Primary-key columns identify a row for UPDATE; without one, editing
        // is disabled.
        let meta = imp.columns.borrow();
        let pk_cols: Vec<(usize, String)> = result
            .columns
            .iter()
            .enumerate()
            .filter(|(_, c)| meta.iter().any(|m| m.name == c.name && m.is_primary_key))
            .map(|(i, c)| (i, c.name.clone()))
            .collect();
        let editable = !pk_cols.is_empty();
        let meta_clone = meta.clone();
        drop(meta);
        drop(imp.pk_cols.replace(pk_cols));

        // Header click re-fetches with a new ORDER BY; cell double-click edits.
        let on_sort: Rc<dyn Fn(usize)> = {
            let weak = self.downgrade();
            Rc::new(move |idx| {
                if let Some(this) = weak.upgrade() {
                    this.sort_by(idx);
                }
            })
        };
        let on_edit: Rc<dyn Fn(RowObject, usize, String, gtk::Widget)> = {
            let weak = self.downgrade();
            Rc::new(move |row, idx, col, anchor| {
                if let Some(this) = weak.upgrade() {
                    this.edit_cell(&row, idx, &col, &anchor);
                }
            })
        };
        let on_context: ContextFn = {
            let weak = self.downgrade();
            Rc::new(move |row, idx, anchor| {
                if let Some(this) = weak.upgrade() {
                    this.show_row_menu(&row, idx, &anchor);
                }
            })
        };

        imp.result_grid.set_result(
            result,
            GridOpts {
                meta: meta_clone,
                sort: *imp.sort.borrow(),
                editable,
                on_sort: Some(on_sort),
                on_edit: Some(on_edit),
                on_context: Some(on_context),
            },
        );
        drop(guard);
        self.update_summary();
    }

    /// Populate the Structure tab from column metadata.
    fn build_structure(&self) {
        let imp = self.imp();
        for row in imp.structure_rows.take() {
            imp.structure_group.remove(&row);
        }
        let mut rows = Vec::new();
        for c in imp.columns.borrow().iter() {
            let mut bits = vec![c.data_type.clone()];
            if !c.nullable {
                bits.push("not null".into());
            }
            if let Some(d) = &c.default {
                bits.push(format!("default {d}"));
            }
            if let Some(r) = &c.references {
                bits.push(format!("→ {r}"));
            }
            let row = adw::ActionRow::builder()
                .title(&c.name)
                .subtitle(&bits.join("  ·  "))
                .build();
            if c.is_primary_key {
                let icon = gtk::Image::from_icon_name("dialog-password-symbolic");
                icon.add_css_class("accent");
                icon.set_tooltip_text(Some("Primary key"));
                row.add_prefix(&icon);
            } else if c.is_foreign_key {
                let icon = gtk::Image::from_icon_name("insert-link-symbolic");
                icon.add_css_class("dim-label");
                icon.set_tooltip_text(Some("Foreign key"));
                row.add_prefix(&icon);
            }
            imp.structure_group.add(&row);
            rows.push(row);
        }
        imp.structure_rows.replace(rows);
    }

    /// Populate the Indexes tab.
    fn build_indexes(&self, indexes: Vec<IndexInfo>) {
        let imp = self.imp();
        for row in imp.indexes_rows.take() {
            imp.indexes_group.remove(&row);
        }
        let mut rows = Vec::new();

        if indexes.is_empty() {
            let row = adw::ActionRow::builder()
                .title("No indexes")
                .subtitle("This table has no indexes.")
                .build();
            row.add_css_class("dim-label");
            imp.indexes_group.add(&row);
            rows.push(row);
        } else {
            for ix in &indexes {
                let mut tags = Vec::new();
                if ix.primary {
                    tags.push("PRIMARY".to_string());
                } else if ix.unique {
                    tags.push("UNIQUE".to_string());
                }
                tags.push(ix.method.clone());
                let subtitle = format!("{}  ·  ({})", tags.join("  ·  "), ix.columns);

                let row = adw::ActionRow::builder()
                    .title(&ix.name)
                    .subtitle(&subtitle)
                    .tooltip_text(&ix.definition)
                    .build();
                let icon = if ix.primary {
                    "dialog-password-symbolic"
                } else if ix.unique {
                    "emblem-ok-symbolic"
                } else {
                    "view-sort-ascending-symbolic"
                };
                let image = gtk::Image::from_icon_name(icon);
                if ix.primary {
                    image.add_css_class("accent");
                } else {
                    image.add_css_class("dim-label");
                }
                row.add_prefix(&image);
                imp.indexes_group.add(&row);
                rows.push(row);
            }
        }
        imp.indexes_rows.replace(rows);
    }

    fn pk_pairs(&self, row: &RowObject) -> Vec<(String, String)> {
        self.imp()
            .pk_cols
            .borrow()
            .iter()
            .map(|(i, name)| (name.clone(), row.display(*i)))
            .collect()
    }

    /// Pop up a one-field editor anchored to a cell; commit runs an UPDATE.
    fn edit_cell(&self, row: &RowObject, idx: usize, col: &str, anchor: &gtk::Widget) {
        let pk = self.pk_pairs(row);
        if pk.is_empty() {
            return;
        }
        let schema = self.schema();
        let table = self.table();
        let old = row.display(idx);

        let entry = gtk::Entry::builder().text(&old).hexpand(true).build();
        let save = gtk::Button::with_label("Save");
        save.add_css_class("suggested-action");
        let title = gtk::Label::builder()
            .label(format!("Edit {col}"))
            .xalign(0.0)
            .css_classes(["heading"])
            .build();
        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        hbox.append(&entry);
        hbox.append(&save);
        let vbox = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_top(8)
            .margin_bottom(8)
            .margin_start(8)
            .margin_end(8)
            .build();
        vbox.append(&title);
        vbox.append(&hbox);

        let popover = gtk::Popover::new();
        popover.set_child(Some(&vbox));
        popover.set_parent(anchor);
        popover.connect_closed(|p| p.unparent());

        let this = self.clone();
        let row = row.clone();
        let col = col.to_string();
        let entry_c = entry.clone();
        let popover_c = popover.clone();
        let anchor_c = anchor.clone();
        let commit: std::rc::Rc<dyn Fn()> = std::rc::Rc::new(move || {
            let text = entry_c.text().to_string();
            if text == old {
                popover_c.popdown();
                return;
            }
            let new_val = (!text.is_empty()).then(|| text.clone());
            let sql = build_update(&schema, &table, &col, new_val.as_deref(), &pk);
            let conn = this.conn();
            let rx = runtime::spawn(async move { conn.execute(&sql).await });

            let (this, row, anchor, popover) =
                (this.clone(), row.clone(), anchor_c.clone(), popover_c.clone());
            glib::spawn_future_local(async move {
                match rx.recv().await {
                    Ok(Ok(_)) => {
                        let empty = text.is_empty();
                        row.set(idx, if empty { Value::Null } else { Value::Text(text.clone()) });
                        if let Some(label) = anchor.downcast_ref::<gtk::Label>() {
                            label.set_text(if empty { "NULL" } else { &text });
                            if empty {
                                label.add_css_class("dim-label");
                            } else {
                                label.remove_css_class("dim-label");
                            }
                        }
                        popover.popdown();
                    }
                    Ok(Err(e)) => this.imp().summary.set_text(&format!("update failed: {e}")),
                    Err(_) => {}
                }
            });
        });

        save.connect_clicked(glib::clone!(
            #[strong]
            commit,
            move |_| commit()
        ));
        entry.connect_activate(glib::clone!(
            #[strong]
            commit,
            move |_| commit()
        ));

        popover.popup();
        entry.grab_focus();
    }

    /// Right-click menu for a cell: view the full value, copy it, or delete the
    /// row (when the table has a primary key).
    fn show_row_menu(&self, row: &RowObject, idx: usize, anchor: &gtk::Widget) {
        let has_pk = !self.imp().pk_cols.borrow().is_empty();

        let bx = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let popover = gtk::Popover::new();
        popover.set_parent(anchor);
        popover.set_autohide(true);
        popover.set_child(Some(&bx));
        popover.connect_closed(|p| p.unparent());

        let menu_button = |label: &str| {
            let b = gtk::Button::with_label(label);
            b.add_css_class("flat");
            if let Some(child) = b.child().and_downcast::<gtk::Label>() {
                child.set_xalign(0.0);
            }
            b
        };

        let view_btn = menu_button("View value");
        view_btn.connect_clicked(glib::clone!(
            #[weak(rename_to = this)]
            self,
            #[strong]
            popover,
            #[strong(rename_to = row)]
            row,
            #[strong(rename_to = anchor)]
            anchor,
            move |_| {
                popover.popdown();
                this.show_value_detail(&row, idx, &anchor);
            }
        ));
        bx.append(&view_btn);

        let copy_btn = menu_button("Copy value");
        copy_btn.connect_clicked(glib::clone!(
            #[strong]
            popover,
            #[strong(rename_to = row)]
            row,
            #[strong(rename_to = anchor)]
            anchor,
            move |_| {
                anchor.clipboard().set_text(&row.display(idx));
                popover.popdown();
            }
        ));
        bx.append(&copy_btn);

        if has_pk {
            bx.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
            let delete_btn = menu_button("Delete row");
            delete_btn.add_css_class("destructive-action");
            delete_btn.connect_clicked(glib::clone!(
                #[weak(rename_to = this)]
                self,
                #[strong]
                popover,
                #[strong(rename_to = row)]
                row,
                move |_| {
                    popover.popdown();
                    this.confirm_delete(&row);
                }
            ));
            bx.append(&delete_btn);
        }

        popover.popup();
    }

    /// Show a cell's full value in a popover (pretty-printed if it's JSON).
    fn show_value_detail(&self, row: &RowObject, idx: usize, anchor: &gtk::Widget) {
        let raw = row.display(idx);
        let text = pretty_json(&raw).unwrap_or(raw);

        let view = gtk::TextView::new();
        view.set_editable(false);
        view.set_monospace(true);
        view.set_wrap_mode(gtk::WrapMode::WordChar);
        view.set_left_margin(8);
        view.set_right_margin(8);
        view.set_top_margin(8);
        view.set_bottom_margin(8);
        view.buffer().set_text(&text);

        let scroll = gtk::ScrolledWindow::builder()
            .min_content_width(360)
            .min_content_height(160)
            .max_content_width(640)
            .max_content_height(420)
            .propagate_natural_width(true)
            .propagate_natural_height(true)
            .child(&view)
            .build();

        let popover = gtk::Popover::new();
        popover.set_parent(anchor);
        popover.set_child(Some(&scroll));
        popover.connect_closed(|p| p.unparent());
        popover.popup();
    }

    /// Confirm and delete the row identified by its primary key.
    fn confirm_delete(&self, row: &RowObject) {
        let pk = self.pk_pairs(row);
        if pk.is_empty() {
            return;
        }
        let schema = self.schema();
        let table = self.table();
        let descr = pk
            .iter()
            .map(|(c, v)| format!("{c} = {v}"))
            .collect::<Vec<_>>()
            .join(", ");

        let root = self.root().and_downcast::<gtk::Window>();
        let dialog = adw::MessageDialog::new(
            root.as_ref(),
            Some("Delete row?"),
            Some(&format!("This permanently deletes the row where {descr}.")),
        );
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("delete", "Delete");
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");

        let this = self.clone();
        dialog.connect_response(None, move |dialog, resp| {
            if resp != "delete" {
                return;
            }
            let sql = build_delete(&schema, &table, &pk);
            let conn = this.conn();
            let rx = runtime::spawn(async move { conn.execute(&sql).await });
            let this = this.clone();
            glib::spawn_future_local(async move {
                match rx.recv().await {
                    Ok(Ok(_)) => this.reload(),
                    Ok(Err(e)) => this.imp().summary.set_text(&format!("delete failed: {e}")),
                    Err(_) => {}
                }
            });
            dialog.close();
        });
        dialog.present();
    }

    fn sort_by(&self, idx: usize) {
        let next = match *self.imp().sort.borrow() {
            Some((i, asc)) if i == idx => Some((idx, !asc)),
            _ => Some((idx, true)),
        };
        self.imp().sort.replace(next);
        // Re-fetch from the first page with the new ORDER BY.
        self.load();
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

    #[template_callback]
    fn on_add_row_clicked(&self) {
        let columns = self.imp().columns.borrow().clone();
        if columns.is_empty() {
            return;
        }
        let schema = self.schema();
        let table = self.table();

        // One entry per column; empty entries are omitted from the INSERT so
        // defaults / serials / nullable columns take over.
        let group = adw::PreferencesGroup::new();
        let entries: Vec<(String, adw::EntryRow)> = columns
            .iter()
            .map(|c| {
                let row = adw::EntryRow::new();
                row.set_title(&format!("{} ({})", c.name, c.data_type));

                // Required = NOT NULL with no default; everything else can be
                // left empty (DEFAULT / serial / NULL applies).
                let required = !c.nullable && c.default.is_none();
                let tag = gtk::Label::new(Some(if required { "required" } else { "optional" }));
                tag.add_css_class("caption");
                tag.add_css_class(if required { "accent" } else { "dim-label" });
                tag.set_valign(gtk::Align::Center);
                row.add_suffix(&tag);

                let mut hint = if required {
                    "Required".to_string()
                } else {
                    "Optional — leave empty for ".to_string()
                };
                if !required {
                    hint.push_str(if c.default.is_some() { "the default" } else { "NULL" });
                }
                row.set_tooltip_text(Some(&hint));

                group.add(&row);
                (c.name.clone(), row)
            })
            .collect();

        let clamp = adw::Clamp::builder()
            .maximum_size(520)
            .margin_top(16)
            .margin_bottom(16)
            .margin_start(12)
            .margin_end(12)
            .child(&group)
            .build();
        let scrolled = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .child(&clamp)
            .build();
        let toast_overlay = adw::ToastOverlay::new();
        toast_overlay.set_child(Some(&scrolled));

        let header = adw::HeaderBar::new();
        let cancel = gtk::Button::with_label("Cancel");
        let insert = gtk::Button::with_label("Insert");
        insert.add_css_class("suggested-action");
        header.pack_start(&cancel);
        header.pack_end(&insert);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&toast_overlay));

        let window = adw::Window::builder()
            .modal(true)
            .title(format!("New row · {table}"))
            .default_width(480)
            .default_height(560)
            .content(&toolbar)
            .build();
        if let Some(root) = self.root().and_downcast::<gtk::Window>() {
            window.set_transient_for(Some(&root));
        }

        cancel.connect_clicked(glib::clone!(
            #[weak]
            window,
            move |_| window.close()
        ));

        let this = self.clone();
        insert.connect_clicked(glib::clone!(
            #[weak]
            window,
            #[weak]
            toast_overlay,
            move |btn| {
                let fields: Vec<(String, String)> = entries
                    .iter()
                    .filter_map(|(name, row)| {
                        let text = row.text().to_string();
                        (!text.is_empty()).then(|| (name.clone(), text))
                    })
                    .collect();
                let sql = build_insert(&schema, &table, &fields);
                let conn = this.conn();
                btn.set_sensitive(false);
                let rx = runtime::spawn(async move { conn.execute(&sql).await });

                let this = this.clone();
                glib::spawn_future_local(glib::clone!(
                    #[weak]
                    window,
                    #[weak]
                    toast_overlay,
                    #[weak]
                    btn,
                    async move {
                        match rx.recv().await {
                            Ok(Ok(_)) => {
                                window.close();
                                this.reload();
                            }
                            Ok(Err(e)) => {
                                toast_overlay.add_toast(adw::Toast::new(&e.to_string()));
                                btn.set_sensitive(true);
                            }
                            Err(_) => btn.set_sensitive(true),
                        }
                    }
                ));
            }
        ));

        window.present();
    }

    #[template_callback]
    fn on_search_changed(&self) {
        let term = self.imp().search_entry.text().to_string();
        self.imp().search_term.replace(term);

        // Debounce server round-trips while typing.
        if let Some(id) = self.imp().search_source.take() {
            id.remove();
        }
        let this = self.clone();
        let id = glib::timeout_add_local_once(std::time::Duration::from_millis(300), move || {
            this.imp().search_source.take();
            this.reload();
        });
        self.imp().search_source.replace(Some(id));
    }

    /// Export the rows currently loaded in the grid to a CSV file. Only loaded
    /// pages are written (infinite scroll); the summary reports the count.
    #[template_callback]
    fn on_export_clicked(&self) {
        let imp = self.imp();
        let guard = imp.result.borrow();
        let Some(result) = guard.as_ref() else { return };
        let headers: Vec<String> = result.columns.iter().map(|c| c.name.clone()).collect();
        let ncols = headers.len();
        drop(guard);

        let Some(store) = imp.result_grid.store() else { return };
        let mut rows: Vec<Vec<Option<String>>> = Vec::with_capacity(store.n_items() as usize);
        for i in 0..store.n_items() {
            let row = store.item(i).and_downcast::<RowObject>().unwrap();
            let cells = (0..ncols)
                .map(|c| if row.is_null(c) { None } else { Some(row.display(c)) })
                .collect();
            rows.push(cells);
        }
        let n = rows.len();
        let csv = to_csv(&headers, &rows);

        let dialog = gtk::FileDialog::builder()
            .title("Export to CSV")
            .initial_name(format!("{}.csv", self.table()))
            .build();
        let window = self.root().and_downcast::<gtk::Window>();
        let this = self.clone();
        dialog.save(window.as_ref(), gio::Cancellable::NONE, move |res| {
            let Ok(file) = res else { return };
            let Some(path) = file.path() else { return };
            match std::fs::write(&path, csv.as_bytes()) {
                Ok(()) => this
                    .imp()
                    .summary
                    .set_text(&format!("exported {n} row{} → {}", plural(n), path.display())),
                Err(e) => this.imp().summary.set_text(&format!("export failed: {e}")),
            }
        });
    }

    /// Pop up the column-filter editor anchored to the funnel button.
    #[template_callback]
    fn on_filter_clicked(&self) {
        let columns: Vec<String> =
            self.imp().columns.borrow().iter().map(|c| c.name.clone()).collect();
        if columns.is_empty() {
            return;
        }

        let rows_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        // Shared list of the live condition-row widgets, read back on Apply.
        let conds: std::rc::Rc<RefCell<Vec<CondRow>>> = std::rc::Rc::new(RefCell::new(Vec::new()));

        // Seed from active filters, or one blank row to start.
        let existing = self.imp().filters.borrow().clone();
        let seeds: Vec<Option<Filter>> = if existing.is_empty() {
            vec![None]
        } else {
            existing.into_iter().map(Some).collect()
        };
        for seed in &seeds {
            let row = build_condition_row(&columns, seed.as_ref());
            rows_box.append(&row.widget);
            conds.borrow_mut().push(row);
        }

        let add_button = gtk::Button::builder()
            .child(&adw::ButtonContent::builder().icon_name("list-add-symbolic").label("Add condition").build())
            .css_classes(["flat"])
            .build();
        let columns_for_add = columns.clone();
        add_button.connect_clicked(glib::clone!(
            #[strong]
            conds,
            #[weak]
            rows_box,
            move |_| {
                let row = build_condition_row(&columns_for_add, None);
                rows_box.append(&row.widget);
                conds.borrow_mut().push(row);
            }
        ));

        let clear = gtk::Button::with_label("Clear");
        let apply = gtk::Button::with_label("Apply");
        apply.add_css_class("suggested-action");

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        actions.append(&clear);
        actions.append(&spacer);
        actions.append(&apply);

        let vbox = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .margin_top(10)
            .margin_bottom(10)
            .margin_start(10)
            .margin_end(10)
            .build();
        let title = gtk::Label::builder()
            .label("Filter rows")
            .xalign(0.0)
            .css_classes(["heading"])
            .build();
        vbox.append(&title);
        vbox.append(&rows_box);
        vbox.append(&add_button);
        vbox.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        vbox.append(&actions);

        let popover = gtk::Popover::new();
        popover.set_child(Some(&vbox));
        popover.set_parent(&*self.imp().filter_button);
        popover.connect_closed(|p| p.unparent());

        let this = self.clone();
        let popover_c = popover.clone();
        apply.connect_clicked(glib::clone!(
            #[strong]
            conds,
            move |_| {
                let filters: Vec<Filter> =
                    conds.borrow().iter().filter_map(CondRow::to_filter).collect();
                this.imp().filters.replace(filters);
                this.update_filter_indicator();
                popover_c.popdown();
                this.load();
            }
        ));

        let this = self.clone();
        let popover_c = popover.clone();
        clear.connect_clicked(move |_| {
            this.imp().filters.borrow_mut().clear();
            this.update_filter_indicator();
            popover_c.popdown();
            this.load();
        });

        popover.popup();
    }

    /// Reflect active filters on the funnel button (accent + count tooltip).
    fn update_filter_indicator(&self) {
        let imp = self.imp();
        let n = imp.filters.borrow().len();
        if n == 0 {
            imp.filter_button.remove_css_class("accent");
            imp.filter_button.set_tooltip_text(Some("Filter"));
        } else {
            imp.filter_button.add_css_class("accent");
            imp.filter_button
                .set_tooltip_text(Some(&format!("Filtered ({n})")));
        }
    }
}

/// Widgets backing one row of the filter editor.
struct CondRow {
    widget: gtk::Box,
    column: gtk::DropDown,
    op: gtk::DropDown,
    value: gtk::Entry,
}

impl CondRow {
    /// Read this row into a `Filter`, or `None` if it isn't usable (a
    /// value-taking operator with an empty value).
    fn to_filter(&self) -> Option<Filter> {
        let columns = self.column.model()?.downcast::<gtk::StringList>().ok()?;
        let column = columns.string(self.column.selected())?.to_string();
        let op = FilterOp::ALL[self.op.selected() as usize].0;
        let value = self.value.text().to_string();
        if op.takes_value() && value.is_empty() {
            return None;
        }
        Some(Filter { column, op, value })
    }
}

/// Build one `column op value` row for the filter editor, seeded from an
/// existing filter when given.
fn build_condition_row(columns: &[String], seed: Option<&Filter>) -> CondRow {
    let widget = gtk::Box::new(gtk::Orientation::Horizontal, 6);

    let col_model = gtk::StringList::new(&columns.iter().map(String::as_str).collect::<Vec<_>>());
    let column = gtk::DropDown::builder().model(&col_model).build();

    let op_labels: Vec<&str> = FilterOp::ALL.iter().map(|(_, l)| *l).collect();
    let op_model = gtk::StringList::new(&op_labels);
    let op = gtk::DropDown::builder().model(&op_model).build();

    let value = gtk::Entry::builder().hexpand(true).placeholder_text("value").build();

    if let Some(f) = seed {
        if let Some(pos) = columns.iter().position(|c| c == &f.column) {
            column.set_selected(pos as u32);
        }
        if let Some(pos) = FilterOp::ALL.iter().position(|(o, _)| *o == f.op) {
            op.set_selected(pos as u32);
        }
        value.set_text(&f.value);
    }

    // Unary operators (is null / is not null) don't take a value.
    let value_for_toggle = value.clone();
    let sync_value = move |dd: &gtk::DropDown| {
        let takes = FilterOp::ALL[dd.selected() as usize].0.takes_value();
        value_for_toggle.set_sensitive(takes);
    };
    sync_value(&op);
    op.connect_selected_notify(sync_value);

    widget.append(&column);
    widget.append(&op);
    widget.append(&value);
    CondRow { widget, column, op, value }
}

/// "" for 1, "s" otherwise.
fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Render rows as CSV (RFC 4180-ish): a header line then one line per row,
/// `None` cells empty, fields quoted when they contain a comma/quote/newline.
fn to_csv(headers: &[String], rows: &[Vec<Option<String>>]) -> String {
    let mut out = String::new();
    out.push_str(&headers.iter().map(|h| csv_field(h)).collect::<Vec<_>>().join(","));
    out.push('\n');
    for row in rows {
        let line = row
            .iter()
            .map(|c| c.as_deref().map(csv_field).unwrap_or_default())
            .collect::<Vec<_>>()
            .join(",");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

fn csv_field(v: &str) -> String {
    if v.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", v.replace('"', "\"\""))
    } else {
        v.to_string()
    }
}

/// Double any embedded quotes so an identifier is safe inside `"..."`.
fn quote_ident(ident: &str) -> String {
    ident.replace('"', "\"\"")
}

/// Build the paged SELECT. Adds a case-insensitive WHERE across all columns
/// (cast to text) when a search term is present, an optional ORDER BY, and
/// LIMIT/OFFSET. The term is escaped for both the SQL string literal and LIKE
/// metacharacters; identifiers are quoted.
#[allow(clippy::too_many_arguments)]
fn build_query(
    schema: &str,
    table: &str,
    term: &str,
    columns: &[String],
    filters: &[Filter],
    order: Option<&(String, bool)>,
    limit: usize,
    offset: usize,
) -> String {
    let mut sql = format!(
        "SELECT * FROM \"{}\".\"{}\"",
        quote_ident(schema),
        quote_ident(table)
    );

    // WHERE is the AND of the free-text search (an OR across all columns) and
    // each active column filter.
    let mut clauses: Vec<String> = Vec::new();
    let term = term.trim();
    if !term.is_empty() && !columns.is_empty() {
        let pattern = escape_like(term).replace('\'', "''");
        let conds: Vec<String> = columns
            .iter()
            .map(|c| {
                format!(
                    "CAST(\"{}\" AS text) ILIKE '%{}%' ESCAPE '\\'",
                    quote_ident(c),
                    pattern
                )
            })
            .collect();
        clauses.push(format!("({})", conds.join(" OR ")));
    }
    for f in filters {
        clauses.push(f.to_sql());
    }
    if !clauses.is_empty() {
        sql.push_str(&format!(" WHERE {}", clauses.join(" AND ")));
    }

    if let Some((col, asc)) = order {
        sql.push_str(&format!(
            " ORDER BY \"{}\" {}",
            quote_ident(col),
            if *asc { "ASC" } else { "DESC" }
        ));
    }

    sql.push_str(&format!(" LIMIT {limit} OFFSET {offset}"));
    sql
}

/// Build an INSERT for the supplied (column, value) pairs. Values are emitted
/// as quoted string literals; Postgres coerces them to each column's type.
/// Columns not supplied are omitted, so their DEFAULT (or NULL) applies.
fn build_insert(schema: &str, table: &str, fields: &[(String, String)]) -> String {
    let target = format!("\"{}\".\"{}\"", quote_ident(schema), quote_ident(table));
    if fields.is_empty() {
        return format!("INSERT INTO {target} DEFAULT VALUES");
    }
    let cols: Vec<String> = fields
        .iter()
        .map(|(c, _)| format!("\"{}\"", quote_ident(c)))
        .collect();
    let vals: Vec<String> = fields
        .iter()
        .map(|(_, v)| format!("'{}'", v.replace('\'', "''")))
        .collect();
    format!(
        "INSERT INTO {target} ({}) VALUES ({})",
        cols.join(", "),
        vals.join(", ")
    )
}

/// Build an UPDATE setting one column for the row identified by its primary
/// key. Value and key literals are quoted/escaped; `None` sets NULL.
fn build_update(
    schema: &str,
    table: &str,
    col: &str,
    value: Option<&str>,
    pk: &[(String, String)],
) -> String {
    let set = match value {
        Some(v) => format!("'{}'", v.replace('\'', "''")),
        None => "NULL".to_string(),
    };
    let cond = pk
        .iter()
        .map(|(c, v)| format!("\"{}\" = '{}'", quote_ident(c), v.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(" AND ");
    format!(
        "UPDATE \"{}\".\"{}\" SET \"{}\" = {} WHERE {}",
        quote_ident(schema),
        quote_ident(table),
        quote_ident(col),
        set,
        cond
    )
}

/// Build a DELETE for the row identified by its primary key.
fn build_delete(schema: &str, table: &str, pk: &[(String, String)]) -> String {
    let cond = pk
        .iter()
        .map(|(c, v)| format!("\"{}\" = '{}'", quote_ident(c), v.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(" AND ");
    format!(
        "DELETE FROM \"{}\".\"{}\" WHERE {}",
        quote_ident(schema),
        quote_ident(table),
        cond
    )
}

/// Pretty-print `s` if it parses as JSON, else `None`.
fn pretty_json(s: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(s).ok()?;
    serde_json::to_string_pretty(&value).ok()
}

/// Escape LIKE metacharacters so the term matches literally.
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::build_query;

    #[test]
    fn plain_when_no_term() {
        assert_eq!(
            build_query("public", "orders", "", &["id".to_string()], &[], None, 500, 0),
            "SELECT * FROM \"public\".\"orders\" LIMIT 500 OFFSET 0"
        );
    }

    #[test]
    fn escapes_quotes_and_like_metachars() {
        let q = build_query("public", "t", "a'b%c", &["x".to_string()], &[], None, 500, 0);
        assert!(q.contains(r"ILIKE '%a''b\%c%' ESCAPE '\'"), "got: {q}");
    }

    #[test]
    fn ors_all_columns() {
        let q = build_query("s", "t", "z", &["a".to_string(), "b".to_string()], &[], None, 500, 0);
        assert!(q.contains("\"a\"") && q.contains("\"b\"") && q.contains(" OR "));
    }

    #[test]
    fn filters_and_with_search() {
        use super::{Filter, FilterOp};
        let filters = [
            Filter { column: "status".into(), op: FilterOp::Eq, value: "paid".into() },
            Filter { column: "total".into(), op: FilterOp::Gt, value: "10".into() },
        ];
        let q = build_query("s", "t", "ab", &["status".to_string()], &filters, None, 500, 0);
        assert!(q.contains("WHERE ("), "got: {q}");
        assert!(q.contains(") AND \"status\" = 'paid' AND \"total\" > '10'"), "got: {q}");
    }

    #[test]
    fn csv_headers_nulls_and_quoting() {
        let headers = vec!["id".to_string(), "note".to_string()];
        let rows = vec![
            vec![Some("1".to_string()), Some("a,b".to_string())],
            vec![Some("2".to_string()), None],
            vec![Some("3".to_string()), Some("say \"hi\"".to_string())],
        ];
        let csv = super::to_csv(&headers, &rows);
        assert_eq!(
            csv,
            "id,note\n1,\"a,b\"\n2,\n3,\"say \"\"hi\"\"\"\n"
        );
    }

    #[test]
    fn filter_unary_and_null_ops() {
        use super::{Filter, FilterOp};
        let f = [Filter { column: "note".into(), op: FilterOp::IsNull, value: String::new() }];
        let q = build_query("s", "t", "", &[], &f, None, 500, 0);
        assert!(q.contains("WHERE \"note\" IS NULL"), "got: {q}");
    }

    #[test]
    fn order_and_offset() {
        let order = ("id".to_string(), false);
        let q = build_query("s", "t", "", &[], &[], Some(&order), 500, 1000);
        assert!(q.contains("ORDER BY \"id\" DESC"), "got: {q}");
        assert!(q.ends_with("LIMIT 500 OFFSET 1000"), "got: {q}");
    }

    #[test]
    fn insert_default_values_when_empty() {
        assert_eq!(
            super::build_insert("public", "t", &[]),
            "INSERT INTO \"public\".\"t\" DEFAULT VALUES"
        );
    }

    #[test]
    fn update_sets_and_targets_pk() {
        let pk = [("id".to_string(), "42".to_string())];
        let q = super::build_update("public", "orders", "status", Some("paid"), &pk);
        assert_eq!(
            q,
            "UPDATE \"public\".\"orders\" SET \"status\" = 'paid' WHERE \"id\" = '42'"
        );
    }

    #[test]
    fn update_null_and_composite_pk() {
        let pk = [
            ("a".to_string(), "1".to_string()),
            ("b".to_string(), "x'y".to_string()),
        ];
        let q = super::build_update("s", "t", "note", None, &pk);
        assert_eq!(
            q,
            "UPDATE \"s\".\"t\" SET \"note\" = NULL WHERE \"a\" = '1' AND \"b\" = 'x''y'"
        );
    }

    #[test]
    fn delete_targets_composite_pk() {
        let pk = [
            ("a".to_string(), "1".to_string()),
            ("b".to_string(), "x'y".to_string()),
        ];
        assert_eq!(
            super::build_delete("public", "orders", &pk),
            "DELETE FROM \"public\".\"orders\" WHERE \"a\" = '1' AND \"b\" = 'x''y'"
        );
    }

    #[test]
    fn pretty_json_only_for_json() {
        assert!(super::pretty_json("not json").is_none());
        assert_eq!(super::pretty_json("{\"a\":1}").unwrap(), "{\n  \"a\": 1\n}");
    }

    #[test]
    fn insert_quotes_and_escapes() {
        let fields = [
            ("name".to_string(), "O'Brien".to_string()),
            ("city".to_string(), "NYC".to_string()),
        ];
        assert_eq!(
            super::build_insert("public", "users", &fields),
            "INSERT INTO \"public\".\"users\" (\"name\", \"city\") VALUES ('O''Brien', 'NYC')"
        );
    }
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
