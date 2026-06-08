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

    conn.close().await.expect("close");
}
