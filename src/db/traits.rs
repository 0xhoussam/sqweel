use async_trait::async_trait;

use super::{ColumnInfo, ConnectionConfig, DbError, Relation, ResultSet};

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

    // --- Introspection (sidebar / status bar) -------------------------------
    // Kept on `Connection` for now; will split into a dedicated `Introspect`
    // trait once it grows (columns, indexes, constraints, foreign keys).

    /// Human-readable server version, e.g. "16.2".
    async fn server_version(&self) -> Result<String, DbError>;
    /// User-visible schemas (excludes system schemas).
    async fn schemas(&self) -> Result<Vec<String>, DbError>;
    /// Tables and views in a schema, with fast row-count estimates.
    async fn relations(&self, schema: &str) -> Result<Vec<Relation>, DbError>;
    /// Column metadata for a relation (types, nullability, PK/FK).
    async fn columns(&self, schema: &str, table: &str) -> Result<Vec<ColumnInfo>, DbError>;
}
