use super::Driver;
use super::postgres::PgDriver;

/// All compiled-in database drivers. Add new backends here.
pub fn drivers() -> Vec<Box<dyn Driver>> {
    vec![Box::new(PgDriver)]
}

/// Look up a driver by its stable id (e.g. "postgres").
pub fn driver(id: &str) -> Option<Box<dyn Driver>> {
    drivers().into_iter().find(|d| d.id() == id)
}
