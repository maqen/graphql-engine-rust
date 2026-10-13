//! Merging remote schemas into a role's schema, with V2's conflict checks (spec 08 §5).

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

use super::introspection::BUILTIN_SCALARS;
use super::{RemoteField, RemoteSchemaInfo, RemoteSchemas};
use crate::model::*;

/// The root fields and types one remote contributes.
struct Contribution {
    query_fields: Vec<FieldDef>,
    mutation_fields: Vec<FieldDef>,
    types: Vec<TypeDef>,
}

fn contribution(remote: &Arc<RemoteSchemaInfo>) -> Contribution {
    let doc = &remote.customized;
    let root_fields = |root: Option<&str>| -> Vec<FieldDef> {
        let Some(fields) = root.and_then(|r| doc.fields_of(r)) else {
            return vec![];
        };
        if fields.is_empty() {
            return vec![];
        }
        let root = root.expect("checked");
        match &remote.namespace {
            Some(ns) => vec![FieldDef::new(
                ns.clone(),
                TypeRef::named(root),
                FieldKind::Remote(RemoteField::Namespace(remote.clone())),
            )],
            None => fields
                .values()
                .map(|f| {
                    let original = match &f.kind {
                        FieldKind::Remote(RemoteField::Field { original }) => original.clone(),
                        _ => f.name.clone(),
                    };
                    FieldDef {
                        kind: FieldKind::Remote(RemoteField::Root {
                            remote: remote.clone(),
                            original,
                        }),
                        ..f.clone()
                    }
                })
                .collect(),
        }
    };
    let query_fields = root_fields(Some(&doc.query_root));
    let mutation_fields = root_fields(doc.mutation_root.as_deref());

    // Types reachable from the contributed fields.
    let mut seen: HashSet<&str> = HashSet::new();
    let mut queue: Vec<&str> = vec![];
    for f in query_fields.iter().chain(&mutation_fields) {
        push_field(doc, &mut queue, f);
    }
    let mut types = vec![];
    while let Some(name) = queue.pop() {
        if !seen.insert(name) || name.starts_with("__") || BUILTIN_SCALARS.contains(&name) {
            continue;
        }
        let Some(ty) = doc.get(name) else {
            continue;
        };
        match ty {
            TypeDef::Object(o) => {
                o.fields
                    .values()
                    .for_each(|f| push_field(doc, &mut queue, f));
                queue.extend(o.interfaces.iter().map(|i| type_name_in(doc, i)));
            }
            TypeDef::Interface(i) => {
                i.fields
                    .values()
                    .for_each(|f| push_field(doc, &mut queue, f));
                queue.extend(i.possible_types.iter().map(|t| type_name_in(doc, t)));
            }
            TypeDef::Union(u) => {
                queue.extend(u.possible_types.iter().map(|t| type_name_in(doc, t)))
            }
            TypeDef::InputObject(io) => {
                queue.extend(io.fields.values().map(|f| type_name_in(doc, f.ty.base())));
            }
            TypeDef::Scalar(_) | TypeDef::Enum(_) => {}
        }
        types.push(ty.clone());
    }
    // Root types with a namespace are types of the namespace fields; without one their
    // fields are moved to Hasura's roots, so the root types stay out unless referenced.
    types.sort_by(|a, b| a.name().cmp(b.name()));
    Contribution {
        query_fields,
        mutation_fields,
        types,
    }
}

fn push_field<'a>(doc: &'a super::RemoteDocument, queue: &mut Vec<&'a str>, f: &FieldDef) {
    queue.push(type_name_in(doc, f.ty.base()));
    for a in f.args.values() {
        queue.push(type_name_in(doc, a.ty.base()));
    }
}

/// The document's own copy of a name, so the queue can borrow from the document.
fn type_name_in<'a>(doc: &'a super::RemoteDocument, name: &str) -> &'a str {
    doc.types
        .get_key_value(name)
        .map_or("", |(k, _)| k.as_str())
}

/// Add a remote's root fields and types; on a conflict nothing is added and the reasons
/// are returned. `pending_scalars` are scalars the schema will declare (without a
/// description) when it is finished.
pub(crate) fn try_add(
    schema: &mut Schema,
    pending_scalars: &BTreeSet<String>,
    query_root: &mut ObjectDef,
    mutation_root: &mut ObjectDef,
    remote: &Arc<RemoteSchemaInfo>,
) -> Result<(), Vec<String>> {
    let c = contribution(remote);
    let mut errors = vec![];
    for (root, fields) in [
        (&*query_root, &c.query_fields),
        (&*mutation_root, &c.mutation_fields),
    ] {
        for f in fields {
            match root.fields.get(&f.name) {
                Some(existing) if matches!(existing.kind, FieldKind::Remote(_)) => {
                    errors.push(format!("Duplicate remote field '{}'", f.name));
                }
                Some(_) => {
                    errors.push(format!(
                        "Field cannot be overwritten by remote field '{}'",
                        f.name
                    ));
                }
                None => {}
            }
        }
    }
    for ty in &c.types {
        let name = ty.name();
        let conflict = match schema.get(name) {
            Some(existing) => !same_type(existing, ty),
            None => {
                pending_scalars.contains(name)
                    && !matches!(ty, TypeDef::Scalar(s) if s.description.is_none())
            }
        };
        if conflict {
            errors.push(format!(
                "Conflicting types: Found conflicting definitions for GraphQL type '{name}'."
            ));
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    for f in c.query_fields {
        query_root.fields.insert(f.name.clone(), f);
    }
    for f in c.mutation_fields {
        mutation_root.fields.insert(f.name.clone(), f);
    }
    for ty in c.types {
        schema.add(ty);
    }
    Ok(())
}

/// Add the role's remote schemas (already checked against the admin schema, a superset of
/// every role's) to its schema.
pub(crate) fn add_remote_schemas(
    schema: &mut Schema,
    pending_scalars: &BTreeSet<String>,
    remotes: &RemoteSchemas,
    query_root: &mut ObjectDef,
    mutation_root: &mut ObjectDef,
) {
    for remote in &remotes.schemas {
        if let Err(reasons) = try_add(schema, pending_scalars, query_root, mutation_root, remote) {
            // The admin check should have caught this; a role's table types can differ.
            tracing::warn!(
                remote_schema = %remote.name,
                ?reasons,
                "remote schema left out of a role's schema"
            );
        }
    }
}

/// A remote left out of the schema: its name and the reasons.
pub type Rejected = (Arc<str>, Vec<String>);

/// Check remotes, in metadata order, against a finished schema without remotes (the admin
/// schema of the tables and actions). Returns the accepted remotes and, for the others,
/// their name and the reasons.
pub fn check_conflicts(
    schema: &Schema,
    remotes: Vec<Arc<RemoteSchemaInfo>>,
) -> (RemoteSchemas, Vec<Rejected>) {
    let mut schema = schema.clone();
    let mut take_root = |name: Option<&str>, fallback: &str| -> ObjectDef {
        match name.and_then(|n| schema.types.shift_remove(n)) {
            Some(TypeDef::Object(o)) => o,
            _ => ObjectDef::new(fallback, None),
        }
    };
    let query_type = schema.query_type.clone();
    let mutation_type = schema.mutation_type.clone();
    let mut query_root = take_root(Some(&query_type), &query_type);
    let mut mutation_root = take_root(mutation_type.as_deref(), "mutation_root");
    // The placeholder field of an empty query root is not a real field.
    query_root
        .fields
        .retain(|_, f| !matches!(f.kind, FieldKind::NoQueriesAvailable));
    let mut accepted = RemoteSchemas::default();
    let mut rejected = vec![];
    for remote in remotes {
        match try_add(
            &mut schema,
            &BTreeSet::new(),
            &mut query_root,
            &mut mutation_root,
            &remote,
        ) {
            Ok(()) => accepted.schemas.push(remote),
            Err(reasons) => rejected.push((remote.name.clone(), reasons)),
        }
    }
    (accepted, rejected)
}

fn same_input(a: &InputValueDef, b: &InputValueDef) -> bool {
    a.name == b.name
        && a.description == b.description
        && a.ty == b.ty
        && a.default_value == b.default_value
}

fn same_args(
    a: &indexmap::IndexMap<String, InputValueDef>,
    b: &indexmap::IndexMap<String, InputValueDef>,
) -> bool {
    a.len() == b.len() && a.values().zip(b.values()).all(|(x, y)| same_input(x, y))
}

fn same_fields(
    a: &indexmap::IndexMap<String, FieldDef>,
    b: &indexmap::IndexMap<String, FieldDef>,
) -> bool {
    a.len() == b.len()
        && a.values().zip(b.values()).all(|(x, y)| {
            x.name == y.name
                && x.description == y.description
                && x.ty == y.ty
                && same_args(&x.args, &y.args)
        })
}

/// Structural equality of two type definitions (what they look like in the schema).
pub(crate) fn same_type(a: &TypeDef, b: &TypeDef) -> bool {
    if a.name() != b.name() || a.description() != b.description() {
        return false;
    }
    match (a, b) {
        (TypeDef::Scalar(_), TypeDef::Scalar(_)) => true,
        (TypeDef::Enum(x), TypeDef::Enum(y)) => {
            x.values.len() == y.values.len()
                && x.values
                    .values()
                    .zip(y.values.values())
                    .all(|(p, q)| p.name == q.name && p.description == q.description)
        }
        (TypeDef::InputObject(x), TypeDef::InputObject(y)) => same_args(&x.fields, &y.fields),
        (TypeDef::Object(x), TypeDef::Object(y)) => {
            x.interfaces == y.interfaces && same_fields(&x.fields, &y.fields)
        }
        (TypeDef::Interface(x), TypeDef::Interface(y)) => {
            x.possible_types == y.possible_types && same_fields(&x.fields, &y.fields)
        }
        (TypeDef::Union(x), TypeDef::Union(y)) => x.possible_types == y.possible_types,
        _ => false,
    }
}
