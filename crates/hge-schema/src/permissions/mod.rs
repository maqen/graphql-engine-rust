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
pub mod inherited;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use hge_core::{ADMIN_ROLE, InconsistentObject, RoleName};
use hge_ir::{
    BoolExp, DeletePermission, InsertPermission, RootFields, SelectPermission, TablePermissions,
    UpdatePermission,
};
use hge_metadata::{Columns, Metadata};
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
pub fn resolve_permissions(
    metadata: &Metadata,
    tables: &mut Tables,
    inconsistencies: &mut Vec<InconsistentObject>,
) {
    let defined = defined_roles(metadata);
    let inherited = inherited::order_roles(&metadata.inherited_roles, &defined, inconsistencies);
    let resolved: Vec<_> = {
        let parser = Parser { tables };
        tables
            .ordered
            .iter()
            .chain(&tables.logical_models)
            .map(|t| {
                let mut by_role = resolve_table(&parser, t, inconsistencies);
                for role in &inherited {
                    let explicit = by_role.shift_remove(role.role_name.as_str());
                    let derived =
                        inherited::derive(t, role, &by_role, explicit.as_ref(), inconsistencies);
                    if derived != TablePermissions::default() {
                        by_role.insert(&role.role_name, derived);
                    }
                }
                let mut out: HashMap<RoleName, Arc<TablePermissions>> = by_role
                    .into_iter()
                    .map(|(role, perms)| (RoleName::new(role), Arc::new(perms)))
                    .collect();
                out.insert(RoleName::admin(), Arc::new(admin_permissions(t)));
                out
            })
            .collect()
    };
    tables.by_name.clear();
    let all = tables
        .ordered
        .iter_mut()
        .chain(tables.logical_models.iter_mut());
    for (table, permissions) in all.zip(resolved) {
        let mut info = (**table).clone();
        info.permissions = permissions;
        *table = Arc::new(info);
        tables
            .by_name
            .insert((table.source.clone(), table.name.clone()), table.clone());
    }
}

fn resolve_table<'m>(
    parser: &Parser<'_>,
    table: &'m TableInfo,
    inconsistencies: &mut Vec<InconsistentObject>,
) -> IndexMap<&'m str, TablePermissions> {
    let entry = &table.entry;
    let mut by_role: IndexMap<&str, TablePermissions> = IndexMap::new();
    let mut report = |kind: &str, perm: &dyn erased::Definition, e: PermError| {
        let role = perm.role();
        inconsistencies.push(InconsistentObject::table_object(
            &format!("{kind}_permission"),
            role,
            &table.source,
            &table.name,
            format!(
                "in table \"{}\": in permission for role \"{role}\": {e}",
                table.name.v2_text()
            ),
            perm.definition(kind),
        ));
    };

    for p in &entry.select_permissions {
        if p.role == ADMIN_ROLE {
            report(
                "select",
                p,
                "cannot define permission for admin role".into(),
            );
            continue;
        }
        match select_permission(parser, table, &p.role, &p.permission) {
            Ok(perm) => set_once(&mut by_role.entry(&p.role).or_default().select, perm)
                .unwrap_or_else(|e| report("select", p, e)),
            Err(e) => report("select", p, e),
        }
    }
    for p in &entry.insert_permissions {
        if p.role == ADMIN_ROLE {
            report(
                "insert",
                p,
                "cannot define permission for admin role".into(),
            );
            continue;
        }
        match insert_permission(parser, table, &p.permission) {
            Ok(perm) => set_once(&mut by_role.entry(&p.role).or_default().insert, perm)
                .unwrap_or_else(|e| report("insert", p, e)),
            Err(e) => report("insert", p, e),
        }
    }
    for p in &entry.update_permissions {
        if p.role == ADMIN_ROLE {
            report(
                "update",
                p,
                "cannot define permission for admin role".into(),
            );
            continue;
        }
        match update_permission(parser, table, &p.permission) {
            Ok(perm) => set_once(&mut by_role.entry(&p.role).or_default().update, perm)
                .unwrap_or_else(|e| report("update", p, e)),
            Err(e) => report("update", p, e),
        }
    }
    for p in &entry.delete_permissions {
        if p.role == ADMIN_ROLE {
            report(
                "delete",
                p,
                "cannot define permission for admin role".into(),
            );
            continue;
        }
        match parser.bool_exp(table, &p.permission.filter) {
            Ok(filter) => set_once(
                &mut by_role.entry(&p.role).or_default().delete,
                DeletePermission {
                    filter,
                    backend_only: p.permission.backend_only,
                },
            )
            .unwrap_or_else(|e| report("delete", p, e)),
            Err(e) => report("delete", p, e),
        }
    }

    by_role
}

/// Roles that exist without inheritance: named by any table, function, action or remote
/// schema permission.
fn defined_roles(metadata: &Metadata) -> HashSet<&str> {
    fn json_roles(items: &[serde_json::Value]) -> impl Iterator<Item = &str> {
        items
            .iter()
            .filter_map(|i| i.get("permissions")?.as_array())
            .flatten()
            .filter_map(|p| p.get("role")?.as_str())
    }
    let mut roles = HashSet::new();
    for source in &metadata.sources {
        for t in &source.tables {
            roles.extend(t.select_permissions.iter().map(|p| p.role.as_str()));
            roles.extend(t.insert_permissions.iter().map(|p| p.role.as_str()));
            roles.extend(t.update_permissions.iter().map(|p| p.role.as_str()));
            roles.extend(t.delete_permissions.iter().map(|p| p.role.as_str()));
        }
        // Function permissions do not count (V2's `allRoleNames`); logical models' do.
        roles.extend(crate::native::permission_roles(source));
    }
    roles.extend(json_roles(&metadata.actions));
    roles.extend(json_roles(&metadata.remote_schemas));
    roles
}

mod erased {
    use hge_metadata::Permission as PermissionEntry;

    /// A permission entry of any kind, as its inconsistency definition.
    pub trait Definition {
        fn role(&self) -> &str;
        fn definition(&self, kind: &str) -> serde_json::Value;
    }

    impl<P: serde::Serialize> Definition for PermissionEntry<P> {
        fn role(&self) -> &str {
            &self.role
        }

        /// V2's definitions always carry `comment` and the permission's defaults.
        fn definition(&self, kind: &str) -> serde_json::Value {
            use serde_json::Value;
            let mut def = serde_json::to_value(self).unwrap_or_default();
            if let Some(obj) = def.as_object_mut() {
                obj.entry("comment").or_insert(Value::Null);
                if let Some(Value::Object(perm)) = obj.get_mut("permission") {
                    if kind == "select" {
                        perm.entry("allow_aggregations")
                            .or_insert(Value::Bool(false));
                        perm.entry("computed_fields")
                            .or_insert(Value::Array(vec![]));
                    } else {
                        perm.entry("backend_only").or_insert(Value::Bool(false));
                    }
                }
            }
            def
        }
    }
}

/// Keep the first permission of a kind for a role; later duplicates are inconsistent.
fn set_once<P>(slot: &mut Option<P>, perm: P) -> Result<(), PermError> {
    if slot.is_some() {
        return Err("a permission of this kind is already defined for the role".to_owned());
    }
    *slot = Some(perm);
    Ok(())
}

/// `admin`: every column, no filters, no limit.
fn admin_permissions(table: &TableInfo) -> TablePermissions {
    let columns: Vec<String> = table.columns.keys().cloned().collect();
    TablePermissions {
        select: Some(SelectPermission {
            columns: columns.clone(),
            redactions: IndexMap::new(),
            filter: BoolExp::always_true(),
            limit: None,
            allow_aggregations: true,
            computed_fields: table
                .computed_fields
                .iter()
                .filter(|c| c.scalar_type().is_some())
                .map(|c| c.name.clone())
                .collect(),
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
                "expecting a database column; but, \"{missing}\" is a relationship; permissions for relationships are automatically inferred"
            ));
        }
        return Err(format!("column \"{missing}\" does not exist"));
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
    for name in &p.computed_fields {
        match table.computed_field(name) {
            None => return Err(format!("computed field \"{name}\" does not exist")),
            Some(cf) => {
                if let crate::functions::ComputedReturn::Table(t) = &cf.returns {
                    return Err(format!(
                        "select permissions on computed field \"{name}\" are auto-derived from the permissions on its returning table \"{}\" and cannot be specified manually",
                        t.v2_text()
                    ));
                }
            }
        }
    }
    Ok(SelectPermission {
        query_root_fields: root_fields(&p.query_root_fields, false)?,
        subscription_root_fields: root_fields(&p.subscription_root_fields, true)?,
        columns,
        redactions: IndexMap::new(),
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
