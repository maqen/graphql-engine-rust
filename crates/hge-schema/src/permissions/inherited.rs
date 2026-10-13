//! Inherited roles (`inherited_roles` metadata): a role whose permissions are derived from a
//! set of parent roles.
//!
//! * select: the parents' permissions are combined. Rows are the union of the parents'
//!   rows; a column only some parents grant is redacted (NULL) on rows those parents
//!   cannot see.
//! * insert / update / delete: inherited only when every parent that has the permission
//!   has the same one; otherwise the role gets none and an inconsistency is reported.
//! * A permission defined directly on the inherited role wins over the derived one.

use std::collections::{HashMap, HashSet};

use hge_core::InconsistentObject;
use hge_ir::{BoolExp, SelectPermission, TablePermissions};
use hge_metadata::InheritedRole;
use indexmap::IndexMap;

use crate::tables::TableInfo;

/// Valid inherited roles, parents before children. Invalid ones (self-reference, unknown
/// parents, cycles) are reported and left out.
pub fn order_roles<'m>(
    inherited: &'m [InheritedRole],
    defined: &HashSet<&str>,
    inconsistencies: &mut Vec<InconsistentObject>,
) -> Vec<&'m InheritedRole> {
    let by_name: HashMap<&str, &InheritedRole> = inherited
        .iter()
        .map(|r| (r.role_name.as_str(), r))
        .collect();
    let mut valid: HashMap<&str, &InheritedRole> = HashMap::new();
    for role in inherited {
        let name = role.role_name.as_str();
        if role.role_set.iter().any(|p| p == name) {
            inconsistencies.push(invalid(
                role,
                "an inherited role name cannot be in the role combination".to_owned(),
            ));
            continue;
        }
        let missing: Vec<&str> = role
            .role_set
            .iter()
            .map(String::as_str)
            .filter(|p| !defined.contains(p) && !by_name.contains_key(p))
            .collect();
        if !missing.is_empty() {
            inconsistencies.push(invalid(role, parents_not_found(&missing, name)));
            continue;
        }
        valid.insert(name, role);
    }

    // Depth-first topological order over the inherited parents.
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Visiting,
        Done,
    }
    fn visit<'m>(
        name: &'m str,
        valid: &HashMap<&'m str, &'m InheritedRole>,
        marks: &mut HashMap<&'m str, Mark>,
        stack: &mut Vec<&'m str>,
        order: &mut Vec<&'m InheritedRole>,
        cycles: &mut Vec<Vec<&'m str>>,
    ) {
        match marks.get(name) {
            Some(Mark::Done) => return,
            Some(Mark::Visiting) => {
                let start = stack.iter().position(|n| *n == name).unwrap_or(0);
                let mut cycle = stack[start..].to_vec();
                cycle.push(name);
                cycles.push(cycle);
                return;
            }
            None => {}
        }
        let Some(role) = valid.get(name) else { return };
        marks.insert(name, Mark::Visiting);
        stack.push(name);
        for parent in &role.role_set {
            visit(parent, valid, marks, stack, order, cycles);
        }
        stack.pop();
        marks.insert(name, Mark::Done);
        order.push(role);
    }
    let mut marks = HashMap::new();
    let mut order = vec![];
    let mut cycles = vec![];
    for role in inherited {
        visit(
            &role.role_name,
            &valid,
            &mut marks,
            &mut vec![],
            &mut order,
            &mut cycles,
        );
    }
    if !cycles.is_empty() {
        let in_cycle: HashSet<&str> = cycles.iter().flatten().copied().collect();
        let shown: Vec<String> = cycles
            .iter()
            .map(|c| {
                let names: Vec<String> = c.iter().map(|n| format!("\"{n}\"")).collect();
                format!("[{}]", names.join(","))
            })
            .collect();
        let reason = format!("found cycle(s) in roles: {}", shown.join(", "));
        for role in inherited {
            if in_cycle.contains(role.role_name.as_str()) {
                inconsistencies.push(invalid(role, reason.clone()));
            }
        }
        order.retain(|r| !in_cycle.contains(r.role_name.as_str()));
    }
    // A role built on an invalid inherited role is invalid too (parents come first).
    let mut kept: HashSet<&str> = HashSet::new();
    order.retain(|r| {
        let missing: Vec<&str> = r
            .role_set
            .iter()
            .map(String::as_str)
            .filter(|p| by_name.contains_key(p) && !kept.contains(p))
            .collect();
        if missing.is_empty() {
            kept.insert(&r.role_name);
            return true;
        }
        inconsistencies.push(invalid(r, parents_not_found(&missing, &r.role_name)));
        false
    });
    order
}

fn invalid(role: &InheritedRole, reason: String) -> InconsistentObject {
    InconsistentObject::new(
        "inherited_role",
        format!("inherited role {}", role.role_name),
        reason,
        serde_json::to_value(role).unwrap_or_default(),
    )
}

fn parents_not_found(missing: &[&str], role: &str) -> String {
    format!(
        "the following parent role(s) are not found: {} which are required to construct the inherited role: \"{role}\"",
        missing.join(", ")
    )
}

/// The permissions of inherited role `role` on `table`, given the permissions of all roles
/// resolved so far (parents come first) and the role's own explicit permissions.
pub fn derive(
    table: &TableInfo,
    role: &InheritedRole,
    resolved: &IndexMap<&str, TablePermissions>,
    explicit: Option<&TablePermissions>,
    inconsistencies: &mut Vec<InconsistentObject>,
) -> TablePermissions {
    let parents: Vec<&TablePermissions> = role
        .role_set
        .iter()
        .filter_map(|p| resolved.get(p.as_str()))
        .collect();
    let explicit = explicit.cloned().unwrap_or_default();
    TablePermissions {
        select: explicit
            .select
            .or_else(|| combine_select(table, parents.iter().filter_map(|p| p.select.as_ref()))),
        insert: explicit.insert.or_else(|| {
            let perms = parents.iter().filter_map(|p| p.insert.as_ref());
            inherit("insert", perms, role, table, inconsistencies)
        }),
        update: explicit.update.or_else(|| {
            let perms = parents.iter().filter_map(|p| p.update.as_ref());
            inherit("update", perms, role, table, inconsistencies)
        }),
        delete: explicit.delete.or_else(|| {
            let perms = parents.iter().filter_map(|p| p.delete.as_ref());
            inherit("delete", perms, role, table, inconsistencies)
        }),
    }
}

/// A mutation permission is inherited when every parent that has it has the same one.
/// Otherwise the role gets none and the conflict is reported.
fn inherit<'a, P: Clone + PartialEq + 'a>(
    kind: &str,
    mut perms: impl Iterator<Item = &'a P>,
    role: &InheritedRole,
    table: &TableInfo,
    inconsistencies: &mut Vec<InconsistentObject>,
) -> Option<P> {
    let first = perms.next()?;
    if perms.all(|p| p == first) {
        return Some(first.clone());
    }
    inconsistencies.push(InconsistentObject::conflicting_inherited_permission(
        &role.role_name,
        &format!(
            "{kind} permission on table \"{}\" in source \"{}\"",
            table.display_name(),
            table.source
        ),
        serde_json::json!({
            "permission_type": kind,
            "source": &*table.source,
            "table": table.name,
        }),
    ));
    None
}

fn or(mut items: Vec<BoolExp>) -> BoolExp {
    if items.iter().any(BoolExp::is_true) {
        return BoolExp::always_true();
    }
    match items.len() {
        1 => items.pop().expect("one item"),
        _ => BoolExp::Or(items),
    }
}

/// V2's `CombinedSelPermInfo`.
fn combine_select<'a>(
    table: &TableInfo,
    parents: impl Iterator<Item = &'a SelectPermission>,
) -> Option<SelectPermission> {
    let parents: Vec<&SelectPermission> = parents.collect();
    let first = parents.first()?;
    let mut columns = vec![];
    let mut redactions = IndexMap::new();
    for column in table.columns.keys() {
        let granting: Vec<&&SelectPermission> =
            parents.iter().filter(|p| p.allows_column(column)).collect();
        if granting.is_empty() {
            continue;
        }
        columns.push(column.clone());
        let unredacted = granting.len() == parents.len()
            && granting.iter().all(|p| p.redaction(column).is_none());
        if unredacted {
            continue;
        }
        // A parent that itself redacts the column contributes `redaction AND filter`.
        let condition = or(granting
            .iter()
            .map(|p| match p.redaction(column) {
                None => p.filter.clone(),
                Some(r) if p.filter.is_true() => r.clone(),
                Some(r) => BoolExp::And(vec![r.clone(), p.filter.clone()]),
            })
            .collect());
        if !condition.is_true() {
            redactions.insert(column.clone(), condition);
        }
    }
    let mut computed_fields: Vec<String> = vec![];
    for p in &parents {
        for cf in &p.computed_fields {
            if !computed_fields.contains(cf) {
                computed_fields.push(cf.clone());
            }
        }
    }
    Some(SelectPermission {
        columns,
        redactions,
        filter: or(parents.iter().map(|p| p.filter.clone()).collect()),
        // The largest limit among parents that have one; no limit only if none has one.
        limit: parents.iter().filter_map(|p| p.limit).max(),
        allow_aggregations: parents.iter().any(|p| p.allow_aggregations),
        computed_fields,
        query_root_fields: parents
            .iter()
            .map(|p| p.query_root_fields)
            .fold(first.query_root_fields, |a, b| a.union(b)),
        subscription_root_fields: parents
            .iter()
            .map(|p| p.subscription_root_fields)
            .fold(first.subscription_root_fields, |a, b| a.union(b)),
    })
}
