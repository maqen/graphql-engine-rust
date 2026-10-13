//! What a role may see. Owned by the *Permissions & roles* workstream.
//!
//! Skeleton behaviour (safe subset until that workstream lands):
//! * `admin` sees every tracked table and column.
//! * Any other role sees a table only if it has a select permission whose `filter` is empty
//!   (`{}`), restricted to the permitted columns and `limit`. Tables with row filters are
//!   hidden rather than exposed unfiltered.

use crate::tables::TableInfo;
use hge_core::RoleName;
use hge_metadata::Columns;

/// The select access a role has on a table.
#[derive(Debug, Clone)]
pub struct SelectAccess {
    /// Visible columns (database names), in table order.
    pub columns: Vec<String>,
    pub limit: Option<u64>,
    pub allow_aggregations: bool,
}

impl SelectAccess {
    pub fn allows_column(&self, column: &str) -> bool {
        self.columns.iter().any(|c| c == column)
    }
}

pub fn select_access(table: &TableInfo, role: &RoleName) -> Option<SelectAccess> {
    if role.is_admin() {
        return Some(SelectAccess {
            columns: table.columns.keys().cloned().collect(),
            limit: None,
            allow_aggregations: true,
        });
    }
    let perm = table.entry.select_permission(role.as_str())?;
    let unfiltered = matches!(&perm.filter, serde_json::Value::Object(m) if m.is_empty());
    if !unfiltered {
        return None;
    }
    let columns = table
        .columns
        .keys()
        .filter(|c| match &perm.columns {
            Columns::Star(_) => true,
            Columns::List(cols) => cols.iter().any(|x| x == *c),
        })
        .cloned()
        .collect();
    Some(SelectAccess {
        columns,
        limit: perm.limit,
        allow_aggregations: perm.allow_aggregations,
    })
}
