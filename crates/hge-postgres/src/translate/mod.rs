//! IR → Postgres SQL.
//!
//! All root fields of one operation on one source become a single statement returning one row
//! with one text column per root field, each holding the field's final JSON.

mod aggregate;
mod boolexp;
mod count;
pub mod mutation;
mod relay;
mod select;
mod subscription;

use hge_core::{Code, Error, Session};
use hge_ir::QueryRoot;

use crate::sql::SqlBuilder;

pub use select::Translator;
pub use subscription::translate_multiplexed;

/// The SQL of one root field with every value written as a literal (`/v1/graphql/explain`).
pub fn translate_field_inline(field: &QueryRoot, session: &Session) -> Result<String, Error> {
    let mut t = Translator::with_builder(session, SqlBuilder::inline());
    match field {
        QueryRoot::Select(select) => t.select_many(select, None)?,
        QueryRoot::SelectOne(select) => t.select_one(select, None)?,
        QueryRoot::Count(count) => t.count(count)?,
        QueryRoot::Aggregate(select) => t.select_aggregate(select, None)?,
        QueryRoot::Stream(_) => {
            return Err(Error::new(
                Code::NotSupported,
                "streaming subscriptions are only supported over websockets",
            ));
        }
    }
    Ok(t.b.sql)
}

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
            QueryRoot::Count(count) => t.count(count)?,
            QueryRoot::Stream(_) => {
                return Err(Error::new(
                    Code::NotSupported,
                    "streaming subscriptions are only supported over websockets",
                ));
            }
            QueryRoot::Aggregate(select) => t.select_aggregate(select, None)?,
            QueryRoot::Connection(conn) => t.select_connection(conn, None)?,
        }
        t.b.push(")::text");
    }
    Ok(t.b)
}
