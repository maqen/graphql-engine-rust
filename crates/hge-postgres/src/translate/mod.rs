//! IR → Postgres SQL.
//!
//! All root fields of one operation on one source become a single statement returning one row
//! with one text column per root field, each holding the field's final JSON.

mod boolexp;
pub mod mutation;
mod select;

use hge_core::{Error, Session};
use hge_ir::QueryRoot;

use crate::sql::SqlBuilder;

pub use select::Translator;

/// Translate the root fields of an operation into one SQL statement.
pub fn translate_query(fields: &[&QueryRoot], session: &Session) -> Result<SqlBuilder, Error> {
    let mut t = Translator::new(session);
    t.b.push("SELECT ");
    for (i, field) in fields.iter().enumerate() {
        if i > 0 {
            t.b.push(", ");
        }
        t.b.push("(");
        match field {
            QueryRoot::Select(select) => t.select_many(select, None)?,
            QueryRoot::SelectOne(select) => t.select_one(select, None)?,
        }
        t.b.push(")::text");
    }
    Ok(t.b)
}
