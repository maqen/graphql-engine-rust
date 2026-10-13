//! Actions and custom types: resolution against the tracked tables, their part of each
//! role's schema, and lowering of action root fields to IR. Owned by the *Actions*
//! workstream; execution lives in `hge-actions`.

mod lower;
mod resolve;
mod schema;

pub use lower::{lower_action, lower_async_query};
pub use resolve::*;
pub(crate) use schema::add_root_fields;

/// The fields of an asynchronous action's result object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsyncResultField {
    Id,
    CreatedAt,
    Errors,
    Output,
}
