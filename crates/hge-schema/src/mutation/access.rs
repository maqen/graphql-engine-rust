//! What a role may insert, update and delete.
//!
//! A view of the role's resolved [`hge_ir::TablePermissions`] (owned by the *Permissions &
//! roles* workstream) shaped for mutation schema building and lowering.

use hge_core::RoleName;
use hge_ir::{BoolExp, Value};

use crate::permissions::table_permissions;
use crate::tables::TableInfo;

#[derive(Debug, Clone, Default)]
pub struct MutationAccess {
    pub insert: Option<InsertAccess>,
    pub update: Option<UpdateAccess>,
    pub delete: Option<DeleteAccess>,
}

#[derive(Debug, Clone)]
pub struct InsertAccess {
    /// Insertable columns (database names), in table order.
    pub columns: Vec<String>,
    /// `set`: values inserted regardless of the input (those columns are not in the input).
    pub presets: Vec<(String, Value)>,
    pub check: Option<BoolExp>,
    pub backend_only: bool,
}

#[derive(Debug, Clone)]
pub struct UpdateAccess {
    /// Updatable columns (database names), in table order.
    pub columns: Vec<String>,
    pub presets: Vec<(String, Value)>,
    /// Rows the role may update.
    pub filter: Option<BoolExp>,
    /// Must hold for updated rows.
    pub check: Option<BoolExp>,
    pub backend_only: bool,
}

#[derive(Debug, Clone)]
pub struct DeleteAccess {
    pub filter: Option<BoolExp>,
    pub backend_only: bool,
}

impl InsertAccess {
    /// Columns that appear in the insert input (not preset, not generated).
    pub fn input_columns<'t>(
        &self,
        table: &'t TableInfo,
    ) -> impl Iterator<Item = &'t crate::tables::ColumnInfo> {
        table.columns.values().filter(|c| {
            c.mutable
                && self.columns.contains(&c.name)
                && !self.presets.iter().any(|(p, _)| p == &c.name)
        })
    }
}

impl UpdateAccess {
    /// Columns the role can set (not preset, not generated).
    pub fn input_columns<'t>(
        &self,
        table: &'t TableInfo,
    ) -> impl Iterator<Item = &'t crate::tables::ColumnInfo> {
        table.columns.values().filter(|c| {
            c.mutable
                && self.columns.contains(&c.name)
                && !self.presets.iter().any(|(p, _)| p == &c.name)
        })
    }
}

/// What `role` may insert, update and delete in `table`, from its resolved permissions
/// (`admin` has a synthesised full permission).
pub fn mutation_access(table: &TableInfo, role: &RoleName) -> MutationAccess {
    let Some(p) = table_permissions(table, role) else {
        return MutationAccess::default();
    };
    // An always-true check or filter is no check at all.
    let condition = |e: &BoolExp| (!e.is_true()).then(|| e.clone());
    let presets = |p: &indexmap::IndexMap<String, Value>| {
        p.iter().map(|(c, v)| (c.clone(), v.clone())).collect()
    };
    MutationAccess {
        insert: p.insert.as_ref().map(|i| InsertAccess {
            columns: i.columns.clone(),
            presets: presets(&i.presets),
            check: condition(&i.check),
            backend_only: i.backend_only,
        }),
        update: p.update.as_ref().map(|u| UpdateAccess {
            columns: u.columns.clone(),
            presets: presets(&u.presets),
            filter: condition(&u.filter),
            // Kept even when always true: upserts then skip the insert check on updated
            // rows, as V2 does.
            check: u.check.clone(),
            backend_only: u.backend_only,
        }),
        delete: p.delete.as_ref().map(|d| DeleteAccess {
            filter: condition(&d.filter),
            backend_only: d.backend_only,
        }),
    }
}

/// Mutation access of `role` on every table it can mutate.
pub fn access_map(
    tables: &crate::tables::Tables,
    role: &RoleName,
) -> std::collections::HashMap<(std::sync::Arc<str>, hge_core::QualifiedTable), MutationAccess> {
    tables
        .ordered
        .iter()
        .map(|t| ((t.source.clone(), t.name.clone()), mutation_access(t, role)))
        .filter(|(_, a)| a.insert.is_some() || a.update.is_some() || a.delete.is_some())
        .collect()
}
