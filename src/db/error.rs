use std::fmt;

/// Database-neutral error. Concrete drivers map their native errors into this
/// (e.g. the Postgres impl maps `sqlx::Error`) so nothing driver-specific leaks
/// past the trait boundary.
#[derive(Debug, Clone)]
pub enum DbError {
    /// Bad / incomplete connection parameters.
    Config(String),
    /// Failed to connect, authenticate, or reach the server.
    Connection(String),
    /// A statement failed to execute.
    Query(String),
    /// Operation not supported by this driver.
    Unsupported(String),
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DbError::Config(m) => write!(f, "{m}"),
            DbError::Connection(m) => write!(f, "Connection failed: {m}"),
            DbError::Query(m) => write!(f, "Query failed: {m}"),
            DbError::Unsupported(m) => write!(f, "Unsupported: {m}"),
        }
    }
}

impl std::error::Error for DbError {}
