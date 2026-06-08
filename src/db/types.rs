use std::fmt;

/// Parameters needed to open a connection. Driver-agnostic; the driver decides
/// how to turn these into a real connection string / options.
#[derive(Clone, Debug)]
pub struct ConnectionConfig {
    pub driver_id: String,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    pub password: String,
    pub ssl: bool,
}

/// A single cell value, normalized across databases. Drivers map their native
/// column types into this; the UI/core never sees driver types.
#[derive(Clone, Debug)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => write!(f, "{x}"),
            Value::Text(s) => write!(f, "{s}"),
            Value::Bytes(b) => write!(f, "\\x{} bytes", b.len()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Column {
    pub name: String,
    /// Driver type name for the header subtext (e.g. "int8", "timestamptz").
    pub data_type: String,
}

/// A schema-level relation shown in the sidebar.
#[derive(Clone, Debug)]
pub struct Relation {
    pub name: String,
    pub kind: RelationKind,
    /// Fast row-count estimate (Postgres `pg_class.reltuples`); approximate.
    pub estimated_rows: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelationKind {
    Table,
    View,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub values: Vec<Value>,
}

/// A fully-materialized query result. NOTE: loads the whole result in memory;
/// a streaming variant (`query_stream`) is planned for browsing large tables.
#[derive(Clone, Debug, Default)]
pub struct ResultSet {
    pub columns: Vec<Column>,
    pub rows: Vec<Row>,
}
