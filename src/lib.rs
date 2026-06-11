//! sqweel — a database client and administration tool.
//!
//! The binary (`main.rs`) is a thin shell over this library so the DB layer can
//! be exercised by integration tests without a display server.

pub mod completion;
pub mod db;
pub mod lsp;
pub mod main_view;
pub mod result_grid;
pub mod row_object;
pub mod runtime;
pub mod sql_view;
pub mod store;
pub mod table_view;
pub mod window;
