use async_trait::async_trait;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions, PgRow, PgSslMode};
use sqlx::{AssertSqlSafe, Column as _, Row as _};

use super::{Column, Connection, ConnectionConfig, DbError, Driver, ResultSet, Row, Value};

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
    if let Ok(v) = row.try_get::<Option<String>, _>(i) {
        return v.map(Value::Text).unwrap_or(Value::Null);
    }
    if let Ok(v) = row.try_get::<Option<Vec<u8>>, _>(i) {
        return v.map(Value::Bytes).unwrap_or(Value::Null);
    }
    Value::Null
}
