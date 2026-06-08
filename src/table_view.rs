use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};

use crate::db::{ColumnInfo, Connection, IndexInfo, RelationKind, ResultSet};
use crate::row_object::RowObject;
use crate::runtime;

/// Horizontal cell padding (left + right) plus breathing room on top of the
/// measured text width.
const CELL_PADDING: i32 = 44;
const MIN_COL_WIDTH: i32 = 80;
/// Threshold a column width may not exceed; longer content ellipsizes.
const MAX_COL_WIDTH: i32 = 420;
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
        pub column_view: TemplateChild<gtk::ColumnView>,
        #[template_child]
        pub header_row: TemplateChild<gtk::Box>,
        #[template_child]
        pub header_scroll: TemplateChild<gtk::ScrolledWindow>,
        #[template_child]
        pub grid_scroll: TemplateChild<gtk::ScrolledWindow>,
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
        /// (column index, ascending) of the active client-side sort.
        pub sort: RefCell<Option<(usize, bool)>>,
        pub search_term: RefCell<String>,
        /// Pending debounced search timeout.
        pub search_source: RefCell<Option<glib::SourceId>>,
        /// The live row store, kept so later pages can append.
        pub store: RefCell<Option<gio::ListStore>>,
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

            // Load the next page when scrolled near the bottom.
            let vadj = self.grid_scroll.vadjustment();
            let obj = self.obj();
            vadj.connect_value_changed(glib::clone!(
                #[weak]
                obj,
                move |adj| {
                    let near_bottom =
                        adj.value() + adj.page_size() >= adj.upper() - adj.page_size().max(200.0);
                    if near_bottom {
                        obj.load_more();
                    }
                }
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
        if imp.loading.get() || imp.exhausted.get() || imp.store.borrow().is_none() {
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
                    if let Some(store) = imp.store.borrow().as_ref() {
                        for row in &result.rows {
                            store.append(&RowObject::new(Rc::new(row.values.clone())));
                        }
                    }
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
        let shown = imp.store.borrow().as_ref().map_or(0, |s| s.n_items());
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

        let rows: Vec<&crate::db::Row> = result.rows.iter().collect();

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

        let meta = imp.columns.borrow();
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

            // Hidden native header (title None); our header strip stands in.
            let col = gtk::ColumnViewColumn::new(None, Some(data_factory(idx, numeric, badge, is_pk)));
            col.set_fixed_width(width);
            col.set_resizable(false);
            cv.append_column(&col);

            let active = imp.sort.borrow().filter(|(i, _)| *i == idx).map(|(_, a)| a);
            imp.header_row.append(&self.header_cell(
                idx,
                &column.name,
                &column.data_type,
                width,
                numeric,
                active,
                is_pk,
                is_fk,
            ));
        }
        drop(meta);

        // Hide ColumnView's native header (first child); our strip stands in.
        if let Some(header) = cv.first_child() {
            header.set_visible(false);
        }

        // Seed the store with the first page; later pages append to it.
        let store = gio::ListStore::new::<RowObject>();
        for row in rows {
            store.append(&RowObject::new(Rc::new(row.values.clone())));
        }
        cv.set_model(Some(&gtk::NoSelection::new(Some(store.clone()))));
        drop(guard);
        imp.store.replace(Some(store));
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

    /// A clickable two-line header cell (bold name + dim type) with a sort
    /// chevron when this column is the active sort.
    #[allow(clippy::too_many_arguments)]
    fn header_cell(
        &self,
        idx: usize,
        name: &str,
        dtype: &str,
        width: i32,
        numeric: bool,
        active: Option<bool>,
        is_pk: bool,
        is_fk: bool,
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

fn data_factory(idx: usize, numeric: bool, badge: bool, is_pk: bool) -> gtk::SignalListItemFactory {
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
