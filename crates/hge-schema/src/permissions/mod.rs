//! What each role may do with each table. Owned by the *Permissions & roles* workstream.
//!
//! [`resolve_permissions`] runs once per metadata version, after the tables are resolved: it
//! turns every table's `select/insert/update/delete_permissions` into
//! [`hge_ir::TablePermissions`] (filters as IR boolean expressions with session variable
//! references) and stores them on [`TableInfo::permissions`], keyed by role. `admin` gets a
//! synthesised permission with full access. Invalid permissions are dropped and reported as
//! inconsistencies, as V2 does.
//!
//! Schema building and lowering read the result through [`select_access`] and
//! [`table_permissions`]; mutations use the insert/update/delete parts.

pub mod boolexp;

use std::collections::HashMap;
use std::sync::Arc;

use hge_core::{ADMIN_ROLE, RoleName};
use hge_ir::{
    BoolExp, DeletePermission, InsertPermission, RootFields, SelectPermission, TablePermissions,
    UpdatePermission,
};
use hge_metadata::Columns;
use indexmap::IndexMap;

use crate::tables::{TableInfo, Tables};
use boolexp::{Parser, PermError};

/// The select access a role has on a table.
pub type SelectAccess = SelectPermission;

/// Everything `role` may do with `table`, or `None` if it has no permission at all.
pub fn table_permissions<'t>(
    table: &'t TableInfo,
    role: &RoleName,
) -> Option<&'t TablePermissions> {
    table.permissions.get(role).map(|p| &**p)
}

pub fn select_access(table: &TableInfo, role: &RoleName) -> Option<SelectAccess> {
    table_permissions(table, role)?.select.clone()
}

/// Resolve the permissions of every table. Call after all tables of all sources are resolved
/// (relationships and `_exists` refer to other tables).
pub fn resolve_permissions(tables: &mut Tables, inconsistencies: &mut Vec<String>) {
    let resolved: Vec<_> = {
        let parser = Parser { tables };
        tables
            .ordered
            .iter()
            .map(|t| resolve_table(&parser, t, inconsistencies))
            .collect()
    };
    tables.by_name.clear();
    for (table, permissions) in tables.ordered.iter_mut().zip(resolved) {
        let mut info = (**table).clone();
        info.permissions = permissions;
        *table = Arc::new(info);
        tables
            .by_name
            .insert((table.source.clone(), table.name.clone()), table.clone());
    }
}

fn resolve_table(
    parser: &Parser<'_>,
    table: &TableInfo,
    inconsistencies: &mut Vec<String>,
) -> HashMap<RoleName, Arc<TablePermissions>> {
    let entry = &table.entry;
    let mut by_role: IndexMap<&str, TablePermissions> = IndexMap::new();
    let mut report = |kind: &str, role: &str, e: PermError| {
        inconsistencies.push(format!(
            "{kind} permission of role \"{role}\" on table \"{}\" in source \"{}\": {e}",
            table.display_name(),
            table.source
        ));
    };

    for p in &entry.select_permissions {
        if p.role == ADMIN_ROLE {
            report(
                "select",
                &p.role,
                "cannot define permission for admin role".into(),
            );
            continue;
        }
        match select_permission(parser, table, &p.role, &p.permission) {
            Ok(perm) => by_role.entry(&p.role).or_default().select = Some(perm),
            Err(e) => report("select", &p.role, e),
        }
    }
    for p in &entry.insert_permissions {
        if p.role == ADMIN_ROLE {
            report(
                "insert",
                &p.role,
                "cannot define permission for admin role".into(),
            );
            continue;
        }
        match insert_permission(parser, table, &p.permission) {
            Ok(perm) => by_role.entry(&p.role).or_default().insert = Some(perm),
            Err(e) => report("insert", &p.role, e),
        }
    }
    for p in &entry.update_permissions {
        if p.role == ADMIN_ROLE {
            report(
                "update",
                &p.role,
                "cannot define permission for admin role".into(),
            );
            continue;
        }
        match update_permission(parser, table, &p.permission) {
            Ok(perm) => by_role.entry(&p.role).or_default().update = Some(perm),
            Err(e) => report("update", &p.role, e),
        }
    }
    for p in &entry.delete_permissions {
        if p.role == ADMIN_ROLE {
            report(
                "delete",
                &p.role,
                "cannot define permission for admin role".into(),
            );
            continue;
        }
        match parser.bool_exp(table, &p.permission.filter) {
            Ok(filter) => {
                by_role.entry(&p.role).or_default().delete = Some(DeletePermission {
                    filter,
                    backend_only: p.permission.backend_only,
                })
            }
            Err(e) => report("delete", &p.role, e),
        }
    }

    let mut out: HashMap<RoleName, Arc<TablePermissions>> = by_role
        .into_iter()
        .map(|(role, perms)| (RoleName::new(role), Arc::new(perms)))
        .collect();
    out.insert(RoleName::admin(), Arc::new(admin_permissions(table)));
    out
}

/// `admin`: every column, no filters, no limit.
fn admin_permissions(table: &TableInfo) -> TablePermissions {
    let columns: Vec<String> = table.columns.keys().cloned().collect();
    TablePermissions {
        select: Some(SelectPermission {
            columns: columns.clone(),
            filter: BoolExp::always_true(),
            limit: None,
            allow_aggregations: true,
            computed_fields: vec![],
            query_root_fields: RootFields::ALL,
            subscription_root_fields: RootFields::ALL,
        }),
        insert: Some(InsertPermission {
            columns: columns.clone(),
            check: BoolExp::always_true(),
            presets: IndexMap::new(),
            backend_only: false,
        }),
        update: Some(UpdatePermission {
            columns,
            filter: BoolExp::always_true(),
            check: None,
            presets: IndexMap::new(),
            backend_only: false,
        }),
        delete: Some(DeletePermission {
            filter: BoolExp::always_true(),
            backend_only: false,
        }),
    }
}

/// The permitted columns, in table order.
fn columns(table: &TableInfo, columns: &Columns) -> Result<Vec<String>, PermError> {
    if let Columns::List(names) = columns
        && let Some(missing) = names.iter().find(|c| !table.columns.contains_key(*c))
    {
        if table.relationship_by_name(missing).is_some() {
            return Err(format!(
                "\"{missing}\" is a relationship; only table columns can be listed in a permission"
            ));
        }
        return Err(format!(
            "column \"{missing}\" not found in table \"{}\"",
            table.display_name()
        ));
    }
    Ok(table
        .columns
        .keys()
        .filter(|c| columns.contains(c))
        .cloned()
        .collect())
}

fn select_permission(
    parser: &Parser<'_>,
    table: &TableInfo,
    role: &str,
    p: &hge_metadata::SelectPermission,
) -> Result<SelectPermission, PermError> {
    let columns = columns(table, &p.columns)?;
    let filter = parser.bool_exp(table, &p.filter)?;
    let pk_selectable = table
        .primary_key
        .as_ref()
        .is_some_and(|pk| pk.iter().all(|c| columns.contains(c)));
    let root_fields = |names: &Option<Vec<String>>, stream_allowed: bool| {
        let Some(names) = names else {
            return Ok(RootFields::ALL);
        };
        let mut rf = RootFields::NONE;
        for n in names {
            match n.as_str() {
                "select" => rf.select = true,
                "select_by_pk" => rf.select_by_pk = true,
                "select_aggregate" => rf.select_aggregate = true,
                "select_stream" if stream_allowed => rf.select_stream = true,
                other => return Err(format!("unexpected root field \"{other}\"")),
            }
        }
        if rf.select_by_pk && !pk_selectable {
            return Err(format!(
                "The \"select_by_pk\" field cannot be included in the query_root_fields or subscription_root_fields because the role \"{role}\" does not have access to the primary key of the table \"{}\" in the source \"{}\"",
                table.display_name(),
                table.source
            ));
        }
        if rf.select_aggregate && !p.allow_aggregations {
            return Err("The \"select_aggregate\" root field can only be enabled in the query_root_fields or  the subscription_root_fields when \"allow_aggregations\" is set to true".to_owned());
        }
        Ok(rf)
    };
    Ok(SelectPermission {
        query_root_fields: root_fields(&p.query_root_fields, false)?,
        subscription_root_fields: root_fields(&p.subscription_root_fields, true)?,
        columns,
        filter,
        limit: p.limit,
        allow_aggregations: p.allow_aggregations,
        computed_fields: p.computed_fields.clone(),
    })
}

fn insert_permission(
    parser: &Parser<'_>,
    table: &TableInfo,
    p: &hge_metadata::InsertPermission,
) -> Result<InsertPermission, PermError> {
    let star = Columns::Star(hge_metadata::StarToken);
    let presets = boolexp::presets(table, &p.set)?;
    let mut columns = columns(table, p.columns.as_ref().unwrap_or(&star))?;
    columns.retain(|c| !presets.contains_key(c));
    Ok(InsertPermission {
        columns,
        check: parser.bool_exp(table, &p.check)?,
        presets,
        backend_only: p.backend_only,
    })
}

fn update_permission(
    parser: &Parser<'_>,
    table: &TableInfo,
    p: &hge_metadata::UpdatePermission,
) -> Result<UpdatePermission, PermError> {
    let presets = boolexp::presets(table, &p.set)?;
    let mut columns = columns(table, &p.columns)?;
    columns.retain(|c| !presets.contains_key(c));
    Ok(UpdatePermission {
        columns,
        filter: parser.bool_exp(table, &p.filter)?,
        check: match &p.check {
            None | Some(serde_json::Value::Null) => None,
            Some(c) => Some(parser.bool_exp(table, c)?),
        },
        presets,
        backend_only: p.backend_only,
    })
}

#[cfg(test)]
mod tests;
