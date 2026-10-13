//! Relay (`/v1beta1/relay`): connections and node ids.

use hge_core::ColumnType;

use crate::{Field, OrderDirection, Select, Value};

/// `<table>_connection` (a root field or an array relationship): a page of rows with
/// cursors and page info.
#[derive(Debug, Clone)]
pub struct ConnectionSelect {
    /// The rows: table (or function), `where`, `order_by` (primary key columns appended),
    /// `distinct_on` and permissions. `select.fields` is unused (each `edges.node` carries
    /// its own fields) and `select.args.limit`/`offset` are never set.
    pub select: Select,
    /// The table's primary key columns: the cursor when there is no `order_by`.
    pub primary_key: Vec<(String, ColumnType)>,
    /// Per `select.args.order_by` item, its key path in the cursor object (`["title"]`,
    /// `["author", "name"]`, `["articles_aggregate", "count"]`).
    pub cursor_paths: Vec<Vec<String>>,
    /// `after` / `before`.
    pub split: Option<ConnectionSplit>,
    /// `first` / `last`.
    pub slice: Option<ConnectionSlice>,
    /// Output fields keyed by response key, in query order.
    pub fields: Vec<(String, ConnectionField)>,
}

/// Keep the rows after (or before) a cursor.
#[derive(Debug, Clone)]
pub struct ConnectionSplit {
    pub kind: SplitKind,
    /// The cursor's values, compared lexicographically.
    pub items: Vec<SplitItem>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitKind {
    After,
    Before,
}

#[derive(Debug, Clone)]
pub struct SplitItem {
    pub key: SplitKey,
    pub direction: OrderDirection,
    pub value: Value,
}

/// What a cursor value is compared with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SplitKey {
    /// The value of the `select.args.order_by` item at this index.
    OrderBy(usize),
    /// A primary key column (no `order_by`).
    Column(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionSlice {
    First(u64),
    Last(u64),
}

#[derive(Debug, Clone)]
pub enum ConnectionField {
    Typename(String),
    PageInfo(Vec<(String, PageInfoField)>),
    Edges(Vec<(String, EdgeField)>),
}

#[derive(Debug, Clone)]
pub enum PageInfoField {
    Typename(String),
    StartCursor,
    EndCursor,
    HasNextPage,
    HasPreviousPage,
}

#[derive(Debug, Clone)]
pub enum EdgeField {
    Typename(String),
    Cursor,
    Node(Vec<(String, Field)>),
}

/// The `id` field of a table on the Relay endpoint: base64 of
/// `[1, "<schema>", "<table>", <primary key values>...]`.
#[derive(Debug, Clone)]
pub struct NodeIdField {
    pub schema: String,
    pub table: String,
    pub primary_key: Vec<(String, ColumnType)>,
}

/// `<rel>_connection` on a table. `mapping` is `(this column, remote column)`.
#[derive(Debug, Clone)]
pub struct RelationshipConnection {
    pub mapping: Vec<(String, String)>,
    pub connection: ConnectionSelect,
}
