//! sqweel — a database client and administration tool.
//!
//! The binary (`main.rs`) is a thin shell over this library so the DB layer can
//! be exercised by integration tests without a display server.

pub mod db;
pub mod main_view;
pub mod row_object;
pub mod runtime;
pub mod store;
pub mod table_view;
pub mod window;
