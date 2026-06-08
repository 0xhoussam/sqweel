use async_trait::async_trait;

use super::{ConnectionConfig, DbError, ResultSet};

/// A database backend. One implementor per supported database (Postgres today).
/// Adding a new database = implement this + `Connection`, register in `registry`.
#[async_trait]
pub trait Driver: Send + Sync {
    /// Stable identifier used in configs (e.g. "postgres").
    fn id(&self) -> &'static str;
    /// Human-facing name shown in the Type dropdown (e.g. "PostgreSQL").
    fn display_name(&self) -> &'static str;
    /// Default TCP port, used to prefill the form.
    fn default_port(&self) -> u16;

    /// Open a live connection (pool) using the given config.
    async fn connect(&self, cfg: &ConnectionConfig) -> Result<Box<dyn Connection>, DbError>;

    /// Connect, ping, and tear down — used by the "Test connection" button.
    async fn test(&self, cfg: &ConnectionConfig) -> Result<(), DbError> {
        let conn = self.connect(cfg).await?;
        let result = conn.ping().await;
        let _ = conn.close().await;
        result
    }
}

/// A live connection to a database.
#[async_trait]
pub trait Connection: Send + Sync {
    /// Cheap round-trip to verify the connection is alive.
    async fn ping(&self) -> Result<(), DbError>;
    /// Run a statement that returns no rows; yields rows-affected.
    async fn execute(&self, sql: &str) -> Result<u64, DbError>;
    /// Run a query and collect all rows.
    async fn query(&self, sql: &str) -> Result<ResultSet, DbError>;
    /// Close the connection / drain the pool.
    async fn close(&self) -> Result<(), DbError>;
}
