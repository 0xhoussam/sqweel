use std::cell::RefCell;
use std::rc::Rc;

use gtk::glib;
use gtk::subclass::prelude::*;

use crate::db::Value;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct RowObject {
        pub values: RefCell<Rc<Vec<Value>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RowObject {
        const NAME: &'static str = "SqweelRowObject";
        type Type = super::RowObject;
    }

    impl ObjectImpl for RowObject {}
}

glib::wrapper! {
    /// A single result row, wrapped as a GObject for `Gio.ListStore`.
    /// Holds an `Rc` to the shared cell values; cheap to clone into the store.
    pub struct RowObject(ObjectSubclass<imp::RowObject>);
}

impl RowObject {
    pub fn new(values: Rc<Vec<Value>>) -> Self {
        let obj: Self = glib::Object::new();
        obj.imp().values.replace(values);
        obj
    }

    /// Display string for the cell at `index` ("" if out of range).
    pub fn display(&self, index: usize) -> String {
        self.imp()
            .values
            .borrow()
            .get(index)
            .map(|v| v.to_string())
            .unwrap_or_default()
    }

    /// Whether the cell at `index` is SQL NULL (for styling).
    pub fn is_null(&self, index: usize) -> bool {
        matches!(self.imp().values.borrow().get(index), Some(Value::Null))
    }
}
