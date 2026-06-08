use async_trait::async_trait;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions, PgRow, PgSslMode};
use sqlx::{AssertSqlSafe, Column as _, Row as _, TypeInfo as _};

use std::collections::{HashMap, HashSet};

use super::{
    Column, ColumnInfo, Connection, ConnectionConfig, DbError, Driver, IndexInfo, Relation,
    RelationKind, ResultSet, Row, Value,
};

fn conn_err(e: sqlx::Error) -> DbError {
    DbError::Connection(e.to_string())
}

fn query_err(e: sqlx::Error) -> DbError {
    DbError::Query(e.to_string())
}

pub struct PgDriver;

#[async_trait]
impl Driver for PgDriver {
    fn id(&self) -> &'static str {
        "postgres"
    }

    fn display_name(&self) -> &'static str {
        "PostgreSQL"
    }

    fn default_port(&self) -> u16 {
        5432
    }

    async fn connect(&self, cfg: &ConnectionConfig) -> Result<Box<dyn Connection>, DbError> {
        if cfg.host.trim().is_empty() {
            return Err(DbError::Config("Host is required".into()));
        }
        let opts = PgConnectOptions::new()
            .host(&cfg.host)
            .port(cfg.port)
            .database(&cfg.database)
            .username(&cfg.username)
            .password(&cfg.password)
            .ssl_mode(if cfg.ssl {
                PgSslMode::Require
            } else {
                PgSslMode::Prefer
            });

        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect_with(opts)
            .await
            .map_err(conn_err)?;

        Ok(Box::new(PgConnection { pool }))
    }
}

pub struct PgConnection {
    pool: PgPool,
}

#[async_trait]
impl Connection for PgConnection {
    async fn ping(&self) -> Result<(), DbError> {
        // Acquire a pooled connection and run on it: the `&Pool` executor
        // requires a `'static` query, while `&mut Connection` does not.
        let mut conn = self.pool.acquire().await.map_err(conn_err)?;
        sqlx::query("SELECT 1")
            .execute(&mut *conn)
            .await
            .map(|_| ())
            .map_err(conn_err)
    }

    async fn execute(&self, sql: &str) -> Result<u64, DbError> {
        let mut conn = self.pool.acquire().await.map_err(conn_err)?;
        // sqlx 0.9 needs an owned/`'static` SQL string; the caller's `&str` is
        // arbitrary user SQL, so wrap it in `AssertSqlSafe` after owning it.
        let res = sqlx::query(AssertSqlSafe(sql.to_owned()))
            .execute(&mut *conn)
            .await
            .map_err(query_err)?;
        Ok(res.rows_affected())
    }

    async fn query(&self, sql: &str) -> Result<ResultSet, DbError> {
        let mut conn = self.pool.acquire().await.map_err(conn_err)?;
        let rows = sqlx::query(AssertSqlSafe(sql.to_owned()))
            .fetch_all(&mut *conn)
            .await
            .map_err(query_err)?;

        let columns = rows
            .first()
            .map(|r| {
                r.columns()
                    .iter()
                    .map(|c| Column {
                        name: c.name().to_string(),
                        data_type: c.type_info().name().to_lowercase(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let rows = rows
            .iter()
            .map(|r| Row {
                values: (0..r.len()).map(|i| decode(r, i)).collect(),
            })
            .collect();

        Ok(ResultSet { columns, rows })
    }

    async fn close(&self) -> Result<(), DbError> {
        self.pool.close().await;
        Ok(())
    }

    async fn server_version(&self) -> Result<String, DbError> {
        let mut conn = self.pool.acquire().await.map_err(conn_err)?;
        let row = sqlx::query("SHOW server_version")
            .fetch_one(&mut *conn)
            .await
            .map_err(query_err)?;
        let raw: String = row.try_get(0).map_err(query_err)?;
        // server_version looks like "16.2 (Debian ...)"; keep the number.
        Ok(raw.split_whitespace().next().unwrap_or(&raw).to_string())
    }

    async fn schemas(&self) -> Result<Vec<String>, DbError> {
        let mut conn = self.pool.acquire().await.map_err(conn_err)?;
        let rows = sqlx::query(
            "SELECT schema_name FROM information_schema.schemata \
             WHERE schema_name NOT LIKE 'pg_%' AND schema_name <> 'information_schema' \
             ORDER BY schema_name",
        )
        .fetch_all(&mut *conn)
        .await
        .map_err(query_err)?;
        rows.iter()
            .map(|r| r.try_get::<String, _>(0).map_err(query_err))
            .collect()
    }

    async fn relations(&self, schema: &str) -> Result<Vec<Relation>, DbError> {
        let mut conn = self.pool.acquire().await.map_err(conn_err)?;
        // relkind: r=table, p=partitioned table, v=view, m=materialized view.
        let rows = sqlx::query(
            "SELECT c.relname, c.relkind::text, c.reltuples::bigint \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relkind IN ('r', 'p', 'v', 'm') \
             ORDER BY c.relname",
        )
        .bind(schema)
        .fetch_all(&mut *conn)
        .await
        .map_err(query_err)?;

        Ok(rows
            .iter()
            .map(|r| {
                let name: String = r.try_get(0).unwrap_or_default();
                let relkind: String = r.try_get(1).unwrap_or_default();
                let est: i64 = r.try_get(2).unwrap_or(0);
                let kind = match relkind.as_str() {
                    "v" | "m" => RelationKind::View,
                    _ => RelationKind::Table,
                };
                Relation {
                    name,
                    kind,
                    estimated_rows: est.max(0),
                }
            })
            .collect())
    }

    async fn columns(&self, schema: &str, table: &str) -> Result<Vec<ColumnInfo>, DbError> {
        let mut conn = self.pool.acquire().await.map_err(conn_err)?;

        let col_rows = sqlx::query(
            "SELECT column_name, udt_name, is_nullable, column_default \
             FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = $2 \
             ORDER BY ordinal_position",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(&mut *conn)
        .await
        .map_err(query_err)?;

        // Primary-key columns.
        let pk_rows = sqlx::query(
            "SELECT a.attname FROM pg_index i \
             JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey) \
             WHERE i.indrelid = (quote_ident($1) || '.' || quote_ident($2))::regclass \
             AND i.indisprimary",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(&mut *conn)
        .await
        .map_err(query_err)?;
        let pks: HashSet<String> = pk_rows
            .iter()
            .filter_map(|r| r.try_get::<String, _>(0).ok())
            .collect();

        // Foreign keys -> referenced table.column.
        let fk_rows = sqlx::query(
            "SELECT kcu.column_name, ccu.table_name, ccu.column_name \
             FROM information_schema.table_constraints tc \
             JOIN information_schema.key_column_usage kcu \
               ON kcu.constraint_name = tc.constraint_name \
              AND kcu.constraint_schema = tc.constraint_schema \
             JOIN information_schema.constraint_column_usage ccu \
               ON ccu.constraint_name = tc.constraint_name \
              AND ccu.constraint_schema = tc.constraint_schema \
             WHERE tc.constraint_type = 'FOREIGN KEY' \
               AND tc.table_schema = $1 AND tc.table_name = $2",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(&mut *conn)
        .await
        .map_err(query_err)?;
        let mut fks: HashMap<String, String> = HashMap::new();
        for r in &fk_rows {
            let col: String = r.try_get(0).unwrap_or_default();
            let ref_table: String = r.try_get(1).unwrap_or_default();
            let ref_col: String = r.try_get(2).unwrap_or_default();
            fks.insert(col, format!("{ref_table}.{ref_col}"));
        }

        Ok(col_rows
            .iter()
            .map(|r| {
                let name: String = r.try_get(0).unwrap_or_default();
                let data_type: String = r.try_get::<String, _>(1).unwrap_or_default();
                let nullable = r.try_get::<String, _>(2).map(|s| s == "YES").unwrap_or(true);
                let default = r.try_get::<Option<String>, _>(3).ok().flatten();
                let references = fks.get(&name).cloned();
                ColumnInfo {
                    is_primary_key: pks.contains(&name),
                    is_foreign_key: references.is_some(),
                    references,
                    name,
                    data_type,
                    nullable,
                    default,
                }
            })
            .collect())
    }

    async fn indexes(&self, schema: &str, table: &str) -> Result<Vec<IndexInfo>, DbError> {
        let mut conn = self.pool.acquire().await.map_err(conn_err)?;
        let rows = sqlx::query(
            "SELECT c.relname, i.indisunique, i.indisprimary, am.amname, \
             pg_get_indexdef(i.indexrelid), \
             (SELECT string_agg(pg_get_indexdef(i.indexrelid, k + 1, true), ', ' ORDER BY k) \
              FROM generate_subscripts(i.indkey, 1) AS k) \
             FROM pg_index i \
             JOIN pg_class c ON c.oid = i.indexrelid \
             JOIN pg_class t ON t.oid = i.indrelid \
             JOIN pg_namespace n ON n.oid = t.relnamespace \
             JOIN pg_am am ON am.oid = c.relam \
             WHERE n.nspname = $1 AND t.relname = $2 \
             ORDER BY i.indisprimary DESC, c.relname",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(&mut *conn)
        .await
        .map_err(query_err)?;

        Ok(rows
            .iter()
            .map(|r| IndexInfo {
                name: r.try_get(0).unwrap_or_default(),
                unique: r.try_get(1).unwrap_or(false),
                primary: r.try_get(2).unwrap_or(false),
                method: r.try_get(3).unwrap_or_default(),
                definition: r.try_get(4).unwrap_or_default(),
                columns: r.try_get::<Option<String>, _>(5).ok().flatten().unwrap_or_default(),
            })
            .collect())
    }
}

/// Best-effort decode of a Postgres cell into the neutral `Value`. Tries common
/// types in order; unknown/complex types fall back to text, then Null.
fn decode(row: &PgRow, i: usize) -> Value {
    if let Ok(v) = row.try_get::<Option<i64>, _>(i) {
        return v.map(Value::Int).unwrap_or(Value::Null);
    }
    if let Ok(v) = row.try_get::<Option<i32>, _>(i) {
        return v.map(|x| Value::Int(x as i64)).unwrap_or(Value::Null);
    }
    if let Ok(v) = row.try_get::<Option<i16>, _>(i) {
        return v.map(|x| Value::Int(x as i64)).unwrap_or(Value::Null);
    }
    if let Ok(v) = row.try_get::<Option<f64>, _>(i) {
        return v.map(Value::Float).unwrap_or(Value::Null);
    }
    if let Ok(v) = row.try_get::<Option<f32>, _>(i) {
        return v.map(|x| Value::Float(x as f64)).unwrap_or(Value::Null);
    }
    if let Ok(v) = row.try_get::<Option<bool>, _>(i) {
        return v.map(Value::Bool).unwrap_or(Value::Null);
    }
    // NUMERIC/DECIMAL: rendered as text. `normalized()` drops the padded
    // trailing zeros sqlx produces from pg's base-10000 representation.
    if let Ok(v) = row.try_get::<Option<sqlx::types::BigDecimal>, _>(i) {
        return v
            .map(|d| Value::Text(d.normalized().to_string()))
            .unwrap_or(Value::Null);
    }
    if let Ok(v) = row.try_get::<Option<String>, _>(i) {
        return v.map(Value::Text).unwrap_or(Value::Null);
    }
    if let Ok(v) = row.try_get::<Option<Vec<u8>>, _>(i) {
        return v.map(Value::Bytes).unwrap_or(Value::Null);
    }
    Value::Null
}
