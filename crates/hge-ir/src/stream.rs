//! Streaming subscriptions (`<table>_stream`).

use hge_core::ColumnType;

use crate::{BoolExp, OrderDirection, Select, Value};

/// `author_stream(batch_size:, cursor:, where:)`: the next rows past a cursor column, in
/// cursor order. `select.args.where_` holds the user's `where`; limit, offset and order are
/// unused.
#[derive(Debug, Clone)]
pub struct StreamSelect {
    pub select: Select,
    pub batch_size: u64,
    pub cursor: StreamCursor,
}

#[derive(Debug, Clone)]
pub struct StreamCursor {
    /// Database column name.
    pub column: String,
    pub ty: ColumnType,
    pub direction: OrderDirection,
    /// Where streaming starts (exclusive).
    pub initial_value: Value,
    /// Inherited-role redaction: the cursor column reads as NULL unless this holds.
    pub redact: Option<Box<BoolExp>>,
}
