//! Row counts for the legacy `count` query (`/v1/query`, `/v2/query`).

use hge_core::QualifiedTable;

use crate::BoolExp;

/// `SELECT count(*)` over the rows of a table that pass the filters, or over the distinct
/// combinations of `distinct` columns of those rows (NULLs included, unlike
/// `count(DISTINCT …)`). Produces a JSON number.
#[derive(Debug, Clone)]
pub struct Count {
    pub table: QualifiedTable,
    pub distinct: Vec<String>,
    /// Row filter from permissions, ANDed with `where_`.
    pub permission_filter: Option<BoolExp>,
    pub where_: Option<BoolExp>,
}
