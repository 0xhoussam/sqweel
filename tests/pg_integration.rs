//! End-to-end check of the Postgres driver against a live database.
//!
//! Ignored by default (needs a server). Run with a throwaway container:
//!   docker run -d --name sqweel-pg -e POSTGRES_PASSWORD=marwa \
//!     -e POSTGRES_USER=marwa -e POSTGRES_DB=analytics -p 5432:5432 postgres:16
//!   cargo test --test pg_integration -- --ignored --nocapture

use sqweel::db::{self, ConnectionConfig, RelationKind};

fn config() -> ConnectionConfig {
    ConnectionConfig {
        driver_id: "postgres".into(),
        host: "localhost".into(),
        port: 5432,
        database: "analytics".into(),
        username: "marwa".into(),
        password: "marwa".into(),
        ssl: false,
    }
}

#[test]
#[ignore = "requires a live postgres (see module docs)"]
fn introspect_and_query() {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run());
}

async fn run() {
    let driver = db::driver("postgres").expect("postgres driver");
    let conn = driver.connect(&config()).await.expect("connect");

    let version = conn.server_version().await.expect("version");
    println!("server_version = {version}");
    assert!(version.starts_with("16"));

    let schemas = conn.schemas().await.expect("schemas");
    println!("schemas = {schemas:?}");
    assert!(schemas.contains(&"public".to_string()));

    let relations = conn.relations("public").await.expect("relations");
    println!("relations:");
    for r in &relations {
        println!("  {:?} {} ~{}", r.kind, r.name, r.estimated_rows);
    }
    let orders = relations
        .iter()
        .find(|r| r.name == "orders")
        .expect("orders relation");
    assert_eq!(orders.kind, RelationKind::Table);
    assert!(orders.estimated_rows >= 100, "reltuples estimate populated");

    assert!(
        relations
            .iter()
            .any(|r| r.name == "active_customers" && r.kind == RelationKind::View),
        "views detected"
    );

    let rs = conn
        .query("SELECT * FROM \"public\".\"orders\" LIMIT 5")
        .await
        .expect("query");
    println!(
        "columns = {:?}",
        rs.columns.iter().map(|c| format!("{}:{}", c.name, c.data_type)).collect::<Vec<_>>()
    );
    assert_eq!(rs.rows.len(), 5);
    assert!(rs.columns.iter().any(|c| c.name == "status"));
    assert!(rs.columns.iter().any(|c| c.name == "total" && c.data_type == "numeric"));

    let cols = conn.columns("public", "orders").await.expect("columns");
    println!("columns:");
    for c in &cols {
        println!(
            "  {} {} pk={} fk={} ref={:?}",
            c.name, c.data_type, c.is_primary_key, c.is_foreign_key, c.references
        );
    }
    let id = cols.iter().find(|c| c.name == "id").expect("id column");
    assert!(id.is_primary_key, "id is PK");
    let cust = cols
        .iter()
        .find(|c| c.name == "customer_id")
        .expect("customer_id column");
    assert!(cust.is_foreign_key, "customer_id is FK");
    assert_eq!(cust.references.as_deref(), Some("customers.id"));

    // Server-side search semantics: CAST(col AS text) ILIKE across types.
    let hits = conn
        .query("SELECT * FROM \"public\".\"orders\" WHERE CAST(\"status\" AS text) ILIKE '%paid%' LIMIT 1000")
        .await
        .expect("search by enum");
    assert!(!hits.rows.is_empty(), "found paid orders");

    let by_id = conn
        .query("SELECT * FROM \"public\".\"orders\" WHERE CAST(\"id\" AS text) ILIKE '%20416%' LIMIT 1000")
        .await
        .expect("search by id");
    assert_eq!(by_id.rows.len(), 1, "exact id match via text cast");

    let idx = conn.indexes("public", "orders").await.expect("indexes");
    println!("indexes:");
    for i in &idx {
        println!(
            "  {} unique={} primary={} {} ({})",
            i.name, i.unique, i.primary, i.method, i.columns
        );
    }
    let pk = idx.iter().find(|i| i.primary).expect("primary key index");
    assert!(pk.unique);
    assert_eq!(pk.method, "btree");
    assert!(pk.columns.contains("id"));

    // Pagination: ORDER BY + LIMIT/OFFSET returns disjoint, contiguous pages.
    let page1 = conn
        .query("SELECT id FROM \"public\".\"orders\" ORDER BY \"id\" ASC LIMIT 500 OFFSET 0")
        .await
        .expect("page 1");
    let page2 = conn
        .query("SELECT id FROM \"public\".\"orders\" ORDER BY \"id\" ASC LIMIT 500 OFFSET 500")
        .await
        .expect("page 2");
    assert_eq!(page1.rows.len(), 500, "full first page");
    assert!(!page2.rows.is_empty(), "second page has rows");
    let last_p1 = page1.rows.last().unwrap().values[0].to_string();
    let first_p2 = page2.rows.first().unwrap().values[0].to_string();
    assert_ne!(last_p1, first_p2, "pages do not overlap");

    // PK-less table: ctid gives stable OFFSET pagination.
    let log_cols = conn.columns("public", "logs").await.expect("logs columns");
    assert!(
        log_cols.iter().all(|c| !c.is_primary_key),
        "logs has no primary key"
    );
    let cp1 = conn
        .query("SELECT msg FROM \"public\".\"logs\" ORDER BY \"ctid\" ASC LIMIT 500 OFFSET 0")
        .await
        .expect("ctid page 1");
    let cp2 = conn
        .query("SELECT msg FROM \"public\".\"logs\" ORDER BY \"ctid\" ASC LIMIT 500 OFFSET 500")
        .await
        .expect("ctid page 2");
    assert_eq!(cp1.rows.len(), 500);
    assert!(!cp2.rows.is_empty());
    assert_ne!(
        cp1.rows.last().unwrap().values[0].to_string(),
        cp2.rows.first().unwrap().values[0].to_string(),
        "ctid pages do not overlap"
    );

    // Add-row: serial PK omitted (DEFAULT) + string-literal coercion.
    // Idempotent: clear any leftovers from a previous run first.
    conn.execute("DELETE FROM customers WHERE email = 'addrow@x.io'").await.ok();
    conn.execute("DELETE FROM orders WHERE id = 99999").await.ok();
    conn.execute(
        "INSERT INTO \"public\".\"customers\" (\"name\", \"email\") \
         VALUES ('addrow', 'addrow@x.io')",
    )
    .await
    .expect("insert with default id");
    let n = conn
        .query("SELECT count(*) FROM customers WHERE email = 'addrow@x.io'")
        .await
        .expect("count");
    assert_eq!(n.rows[0].values[0].to_string(), "1");

    // Values passed as text literals coerce to int8 / enum / numeric / timestamptz.
    conn.execute(
        "INSERT INTO \"public\".\"orders\" \
         (\"id\", \"customer_id\", \"status\", \"total\", \"currency\", \"created_at\") \
         VALUES ('99999', '1', 'paid', '10.50', 'EUR', '2026-01-01 00:00:00')",
    )
    .await
    .expect("insert with coercion");
    let t = conn
        .query("SELECT total FROM orders WHERE id = 99999")
        .await
        .expect("read back");
    assert_eq!(t.rows[0].values[0].to_string(), "10.5", "total decoded as {:?}", t.rows[0].values[0]);

    // Cleanup.
    conn.execute("DELETE FROM customers WHERE email = 'addrow@x.io'").await.ok();
    conn.execute("DELETE FROM orders WHERE id = 99999").await.ok();

    // Cell edit: UPDATE one column WHERE primary key, then read it back.
    conn.execute("UPDATE \"public\".\"orders\" SET \"currency\" = 'USD' WHERE \"id\" = '20416'")
        .await
        .expect("update");
    let cur = conn
        .query("SELECT currency FROM orders WHERE id = 20416")
        .await
        .expect("read currency");
    assert_eq!(cur.rows[0].values[0].to_string().trim(), "USD");
    conn.execute("UPDATE \"public\".\"orders\" SET \"currency\" = 'EUR' WHERE \"id\" = '20416'")
        .await
        .ok();

    conn.close().await.expect("close");
}
