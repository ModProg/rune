use crate::collections::HashMap;
use crate::compile::{IntoComponent, Item};

/// The contents of a prelude.
#[derive(Default)]
pub struct Prelude {
    /// Prelude imports.
    prelude: HashMap<Box<str>, Item>,
}

impl Prelude {
    /// Construct a new unit with the default prelude.
    pub(crate) fn with_default_prelude() -> Self {
        let mut this = Self::default();

        this.add_prelude("assert_eq", &["test", "assert_eq"]);
        this.add_prelude("assert", &["test", "assert"]);
        this.add_prelude("bool", &["bool"]);
        this.add_prelude("byte", &["byte"]);
        this.add_prelude("char", &["char"]);
        this.add_prelude("dbg", &["io", "dbg"]);
        this.add_prelude("drop", &["mem", "drop"]);
        this.add_prelude("Err", &["result", "Result", "Err"]);
        this.add_prelude("file", &["macros", "builtin", "file"]);
        this.add_prelude("float", &["float"]);
        this.add_prelude("format", &["fmt", "format"]);
        this.add_prelude("int", &["int"]);
        this.add_prelude("is_readable", &["is_readable"]);
        this.add_prelude("is_writable", &["is_writable"]);
        this.add_prelude("line", &["macros", "builtin", "line"]);
        this.add_prelude("None", &["option", "Option", "None"]);
        this.add_prelude("Object", &["object", "Object"]);
        this.add_prelude("Ok", &["result", "Result", "Ok"]);
        this.add_prelude("Option", &["option", "Option"]);
        this.add_prelude("panic", &["panic"]);
        this.add_prelude("print", &["io", "print"]);
        this.add_prelude("println", &["io", "println"]);
        this.add_prelude("Result", &["result", "Result"]);
        this.add_prelude("Some", &["option", "Option", "Some"]);
        this.add_prelude("String", &["string", "String"]);
        this.add_prelude("stringify", &["stringify"]);
        this.add_prelude("unit", &["unit"]);
        this.add_prelude("Vec", &["vec", "Vec"]);

        this
    }

    /// Access a value from the prelude.
    pub(crate) fn get<'a>(&'a self, name: &str) -> Option<&'a Item> {
        self.prelude.get(name)
    }

    /// Define a prelude item.
    fn add_prelude<I>(&mut self, local: &str, path: I)
    where
        I: IntoIterator,
        I::Item: IntoComponent,
    {
        self.prelude
            .insert(local.into(), Item::with_crate_item("std", path));
    }
}