//! Permissions of one role on one table, resolved from metadata.
//!
//! `hge-schema` resolves the V2 permission metadata (`select_permissions`, …, inherited roles)
//! into these types once per metadata version. Filters and checks are ordinary [`BoolExp`]s
//! whose values may be [`Value::Session`] references, bound per request by the backend.
//! Owned by the *Permissions & roles* workstream; mutations read the insert/update/delete parts.

use indexmap::IndexMap;

use crate::{BoolExp, Value};

/// Everything one role may do with one table. `None` means no permission of that kind.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TablePermissions {
    pub select: Option<SelectPermission>,
    pub insert: Option<InsertPermission>,
    pub update: Option<UpdatePermission>,
    pub delete: Option<DeletePermission>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SelectPermission {
    /// Selectable columns (database names), in table order.
    pub columns: Vec<String>,
    /// Columns and scalar computed fields an inherited role sees only on some rows: the
    /// value is NULL unless the condition holds (V2's `RedactIfFalse`). Fields not listed
    /// are never redacted. One map serves both, as V2 rejects a computed field named like a
    /// column.
    pub redactions: IndexMap<String, BoolExp>,
    /// Row filter (`BoolExp::And(vec![])` when unrestricted).
    pub filter: BoolExp,
    pub limit: Option<u64>,
    pub allow_aggregations: bool,
    /// Selectable scalar computed fields.
    pub computed_fields: Vec<String>,
    /// `query_root_fields`
    pub query_root_fields: RootFields,
    /// `subscription_root_fields`
    pub subscription_root_fields: RootFields,
}

impl SelectPermission {
    pub fn allows_column(&self, column: &str) -> bool {
        self.columns.iter().any(|c| c == column)
    }

    /// The condition under which `column` (or scalar computed field) is visible, if it is
    /// redacted.
    pub fn redaction(&self, column: &str) -> Option<&BoolExp> {
        self.redactions.get(column)
    }

    /// The row filter, or `None` when it lets every row through.
    pub fn row_filter(&self) -> Option<&BoolExp> {
        (!self.filter.is_true()).then_some(&self.filter)
    }
}

/// Which root fields of a table a role gets (`query_root_fields` /
/// `subscription_root_fields`). Absent in metadata means all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootFields {
    pub select: bool,
    pub select_by_pk: bool,
    pub select_aggregate: bool,
    pub select_stream: bool,
}

impl RootFields {
    pub const ALL: RootFields = RootFields {
        select: true,
        select_by_pk: true,
        select_aggregate: true,
        select_stream: true,
    };

    pub const NONE: RootFields = RootFields {
        select: false,
        select_by_pk: false,
        select_aggregate: false,
        select_stream: false,
    };

    /// Union (used when combining parents of an inherited role).
    pub fn union(self, other: RootFields) -> RootFields {
        RootFields {
            select: self.select || other.select,
            select_by_pk: self.select_by_pk || other.select_by_pk,
            select_aggregate: self.select_aggregate || other.select_aggregate,
            select_stream: self.select_stream || other.select_stream,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct InsertPermission {
    /// Columns the role may provide (preset columns excluded), in table order.
    pub columns: Vec<String>,
    /// Evaluated on every inserted row; the mutation fails unless it holds for all of them.
    pub check: BoolExp,
    /// Column presets (`set`), applied to every row; the user cannot provide these columns.
    pub presets: IndexMap<String, Value>,
    pub backend_only: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UpdatePermission {
    /// Columns the role may set (preset columns excluded), in table order.
    pub columns: Vec<String>,
    /// Rows the role may update.
    pub filter: BoolExp,
    /// Evaluated on the updated rows; `None` means no post-update check.
    pub check: Option<BoolExp>,
    pub presets: IndexMap<String, Value>,
    pub backend_only: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeletePermission {
    /// Rows the role may delete.
    pub filter: BoolExp,
    pub backend_only: bool,
}
