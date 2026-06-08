//! Database-agnostic core. Everything DB-related lives behind the traits in
//! `traits`; concrete backends (currently only Postgres) implement them and are
//! registered in `registry`. The rest of the app talks only to `Box<dyn ...>`
//! and the neutral types here — no driver crate ever crosses this boundary.

mod error;
mod registry;
mod traits;
mod types;

pub mod postgres;

pub use error::DbError;
pub use registry::{driver, drivers};
pub use traits::{Connection, Driver};
pub use types::{
    Column, ColumnInfo, ConnectionConfig, Relation, RelationKind, ResultSet, Row, Value,
};
