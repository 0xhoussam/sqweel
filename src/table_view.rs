use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;

use crate::db::{ColumnInfo, Connection, IndexInfo, RelationKind, ResultSet, Value};
use crate::result_grid::{GridOpts, ResultGrid};
use crate::row_object::RowObject;
use crate::runtime;

/// Rows fetched per page (infinite scroll).
const PAGE_SIZE: usize = 500;

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
        let order = self.order_target();
        let sql = build_query(&schema, &table, &term, &names, order.as_ref(), PAGE_SIZE, 0);
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
        let order = self.order_target();
        let offset = imp.offset.get();
        let sql = build_query(&schema, &table, &term, &names, order.as_ref(), PAGE_SIZE, offset);
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

        imp.result_grid.set_result(
            result,
            GridOpts {
                meta: meta_clone,
                sort: *imp.sort.borrow(),
                editable,
                on_sort: Some(on_sort),
                on_edit: Some(on_edit),
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
}

/// Double any embedded quotes so an identifier is safe inside `"..."`.
fn quote_ident(ident: &str) -> String {
    ident.replace('"', "\"\"")
}

/// Build the paged SELECT. Adds a case-insensitive WHERE across all columns
/// (cast to text) when a search term is present, an optional ORDER BY, and
/// LIMIT/OFFSET. The term is escaped for both the SQL string literal and LIKE
/// metacharacters; identifiers are quoted.
fn build_query(
    schema: &str,
    table: &str,
    term: &str,
    columns: &[String],
    order: Option<&(String, bool)>,
    limit: usize,
    offset: usize,
) -> String {
    let mut sql = format!(
        "SELECT * FROM \"{}\".\"{}\"",
        quote_ident(schema),
        quote_ident(table)
    );

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
        sql.push_str(&format!(" WHERE {}", conds.join(" OR ")));
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
            build_query("public", "orders", "", &["id".to_string()], None, 500, 0),
            "SELECT * FROM \"public\".\"orders\" LIMIT 500 OFFSET 0"
        );
    }

    #[test]
    fn escapes_quotes_and_like_metachars() {
        let q = build_query("public", "t", "a'b%c", &["x".to_string()], None, 500, 0);
        assert!(q.contains(r"ILIKE '%a''b\%c%' ESCAPE '\'"), "got: {q}");
    }

    #[test]
    fn ors_all_columns() {
        let q = build_query("s", "t", "z", &["a".to_string(), "b".to_string()], None, 500, 0);
        assert!(q.contains("\"a\"") && q.contains("\"b\"") && q.contains(" OR "));
    }

    #[test]
    fn order_and_offset() {
        let order = ("id".to_string(), false);
        let q = build_query("s", "t", "", &[], Some(&order), 500, 1000);
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
