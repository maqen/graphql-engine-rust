//! Remote schema commands (spec 08 §2, V2's `RemoteSchema/MetadataAPI/Core.hs` and
//! `RemoteSchema/MetadataAPI/Permission.hs`).
//!
//! Remote schemas are kept in the metadata as JSON (`remote_schemas[]`). Introspection and
//! the schema checks happen when the metadata is resolved (`ctx.check`); `add_remote_schema`
//! and `update_remote_schema` also introspect up front, as V2 does, so an unreachable or
//! invalid remote is the command's own error rather than an inconsistency.

use hge_core::{Code, InconsistentObject, QualifiedTable};
use serde_json::{Map, Value};

use super::args::{Obj, boolean, field, missing, object, required_string, text};
use crate::ctx::{Check, Ctx};
use crate::response::{ApiError, ApiResult, already_exists, not_exists, parse_failed, success};
use crate::runtime::{RemoteReload, Runtime};

const ARGS: &str = "$.args";

fn entry_name(entry: &Value) -> Option<&str> {
    entry.get("name").and_then(Value::as_str)
}

fn index(ctx_md: &[Value], name: &str) -> Option<usize> {
    ctx_md.iter().position(|e| entry_name(e) == Some(name))
}

/// Is the remote schema in the metadata and consistent (V2's `scRemoteSchemas`)?
fn is_consistent<R: Runtime>(ctx: &Ctx<'_, R>, name: &str) -> bool {
    let id = format!("remote_schema {name}");
    index(&ctx.metadata.remote_schemas, name).is_some()
        && !ctx
            .inconsistencies
            .iter()
            .any(|o| o.kind == "remote_schema" && o.name == id)
}

fn own_check(name: &str) -> Check {
    Check::object("remote_schema", format!("remote_schema {name}"))
}

/// V2's `withNewInconsistentObjsCheck`: a new inconsistency is a 500.
async fn check_no_new<R: Runtime>(ctx: &mut Ctx<'_, R>) -> ApiResult<()> {
    ctx.check(Check::NoNew).await.map_err(|mut e| {
        if e.error.code == Code::Unexpected {
            e.status = 500;
            e.error.message = "cannot continue due to newly found inconsistent metadata".into();
        }
        e
    })
}

/// `{name, definition, comment?}` of `add_remote_schema` and `update_remote_schema`, as the
/// metadata entry to store (without permissions or relationships).
fn schema_args(args: &Value) -> ApiResult<(String, Map<String, Value>)> {
    let obj = object(args, ARGS)?;
    let name = required_string(obj, "name", ARGS, "Text")?;
    let definition = obj
        .get("definition")
        .ok_or_else(|| missing(ARGS, "definition"))?;
    let definition = object(definition, "$.args.definition")?.clone();
    let comment = text(obj, "comment", ARGS)?;
    let mut entry = Map::new();
    entry.insert("name".into(), Value::String(name.clone()));
    entry.insert("definition".into(), Value::Object(definition));
    if let Some(c) = comment {
        entry.insert("comment".into(), Value::String(c));
    }
    Ok((name, entry))
}

/// `add_remote_schema {name, definition, comment?}`.
pub async fn add_remote_schema<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let (name, entry) = schema_args(args)?;
    // Inconsistent remote schemas count too (V2's `getAllRemoteSchemas`).
    if index(&ctx.metadata.remote_schemas, &name).is_some() {
        return Err(already_exists(format!(
            "remote schema with name \"{name}\" already exists"
        )));
    }
    let entry = Value::Object(entry);
    ctx.rt.validate_remote_schema(&entry, true).await?;
    ctx.metadata.remote_schemas.push(entry);
    ctx.check(own_check(&name)).await?;
    Ok(success())
}

/// `update_remote_schema {name, definition, comment?}`: replaces the definition and the
/// comment and keeps the permissions. Like V2, the remote schema's own remote
/// relationships are dropped.
pub async fn update_remote_schema<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let (name, mut entry) = schema_args(args)?;
    let Some(i) = index(&ctx.metadata.remote_schemas, &name) else {
        return Err(not_exists(format!(
            "remote schema with name \"{name}\" doesn't exist"
        )));
    };
    if let Some(permissions) = ctx.metadata.remote_schemas[i]
        .get("permissions")
        .filter(|p| p.as_array().is_some_and(|a| !a.is_empty()))
    {
        entry.insert("permissions".into(), permissions.clone());
    }
    // V2 introspects up front only when the URL changed; the rebuild introspects anyway.
    let url = |e: &Value| {
        let def = e.get("definition");
        (
            def.and_then(|d| d.get("url")).cloned(),
            def.and_then(|d| d.get("url_from_env")).cloned(),
        )
    };
    let entry = Value::Object(entry);
    let url_changed = url(&entry) != url(&ctx.metadata.remote_schemas[i]);
    ctx.rt.validate_remote_schema(&entry, url_changed).await?;
    ctx.metadata.remote_schemas[i] = entry;
    ctx.check(own_check(&name)).await?;
    Ok(success())
}

/// An object that refers to a remote schema, as V2's `reportSchemaObj` names it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Dependent {
    /// A table's remote relationship.
    Table {
        source: String,
        table: QualifiedTable,
        name: String,
    },
    /// A remote relationship of another remote schema's type.
    RemoteSchema {
        remote: String,
        type_name: String,
        name: String,
    },
}

impl Dependent {
    fn report(&self) -> String {
        match self {
            Dependent::Table {
                source,
                table,
                name,
            } => format!(
                "remote relationship {}.{name} in source \"{source}\"",
                table.v2_text()
            ),
            Dependent::RemoteSchema {
                remote,
                type_name,
                name,
            } => format!("remote_schema_remote_relationship {remote}.{type_name}.{name}"),
        }
    }
}

/// The remote schema a remote relationship definition points at, in either format
/// (`to_remote_schema: {remote_schema}` or the legacy top-level `remote_schema`).
fn target_remote(definition: &Value) -> Option<&str> {
    definition
        .pointer("/to_remote_schema/remote_schema")
        .or_else(|| definition.get("remote_schema"))
        .and_then(Value::as_str)
}

fn dependents(md: &hge_metadata::Metadata, remote: &str) -> Vec<Dependent> {
    let mut out = vec![];
    for source in &md.sources {
        for table in &source.tables {
            for rel in &table.remote_relationships {
                if rel.get("definition").and_then(target_remote) == Some(remote) {
                    out.push(Dependent::Table {
                        source: source.name.clone(),
                        table: table.table.clone(),
                        name: entry_name(rel).unwrap_or_default().to_owned(),
                    });
                }
            }
        }
    }
    for rs in &md.remote_schemas {
        let owner = entry_name(rs).unwrap_or_default();
        if owner == remote {
            continue;
        }
        for ty in rs
            .get("remote_relationships")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let type_name = ty
                .get("type_name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            for rel in ty
                .get("relationships")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if rel.get("definition").and_then(target_remote) == Some(remote) {
                    out.push(Dependent::RemoteSchema {
                        remote: owner.to_owned(),
                        type_name: type_name.to_owned(),
                        name: entry_name(rel).unwrap_or_default().to_owned(),
                    });
                }
            }
        }
    }
    out.sort();
    out
}

/// Remove remote relationships that point at `remote`.
fn drop_dependents(md: &mut hge_metadata::Metadata, remote: &str) {
    let points_here = |rel: &Value| rel.get("definition").and_then(target_remote) == Some(remote);
    for source in &mut md.sources {
        for table in &mut source.tables {
            table.remote_relationships.retain(|r| !points_here(r));
        }
    }
    for rs in &mut md.remote_schemas {
        let Value::Object(entry) = rs else {
            continue;
        };
        let Some(Value::Array(types)) = entry.get_mut("remote_relationships") else {
            continue;
        };
        for ty in types.iter_mut() {
            if let Some(Value::Array(rels)) = ty.get_mut("relationships") {
                rels.retain(|r| !points_here(r));
            }
        }
        types.retain(|ty| {
            ty.get("relationships")
                .and_then(Value::as_array)
                .is_some_and(|r| !r.is_empty())
        });
        if types.is_empty() {
            entry.remove("remote_relationships");
        }
    }
}

/// `remove_remote_schema {name, cascade?}`. Only consistent remote schemas can be removed
/// (an inconsistent one goes with `drop_inconsistent_metadata`).
pub async fn remove_remote_schema<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let obj = object(args, ARGS)?;
    let name = required_string(obj, "name", ARGS, "Text")?;
    let cascade = boolean(obj, "cascade", ARGS)?.unwrap_or(false);
    if !is_consistent(ctx, &name) {
        return Err(not_exists("no such remote schema"));
    }
    let deps = dependents(&ctx.metadata, &name);
    if !deps.is_empty() && !cascade {
        let mut list: Vec<String> = deps.iter().map(Dependent::report).collect();
        list.sort();
        return Err(ApiError::bad(
            Code::DependencyError,
            format!(
                "cannot drop due to the following dependent objects: {}",
                list.join(", ")
            ),
        ));
    }
    drop_dependents(&mut ctx.metadata, &name);
    ctx.metadata
        .remote_schemas
        .retain(|e| entry_name(e) != Some(name.as_str()));
    check_no_new(ctx).await?;
    Ok(success())
}

/// `reload_remote_schema {name}`: introspect the remote again.
pub async fn reload_remote_schema<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let name = name_arg(args)?;
    if index(&ctx.metadata.remote_schemas, &name).is_none() {
        return Err(not_exists(format!(
            "remote schema with name \"{name}\" does not exist"
        )));
    }
    ctx.reload_remotes
        .merge(RemoteReload::Only([name.clone()].into_iter().collect()));
    ctx.invalidations.remote_schemas.push(name);
    check_no_new(ctx).await?;
    Ok(success())
}

/// `introspect_remote_schema {name}`: the remote's own answer to the introspection query.
pub async fn introspect_remote_schema<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let name = name_arg(args)?;
    let not_found = || not_exists(format!("remote schema: \"{name}\" not found"));
    if !is_consistent(ctx, &name) {
        return Err(not_found());
    }
    let raw = ctx
        .rt
        .remote_introspection(ctx.prepared.as_ref(), &name)
        .ok_or_else(not_found)?;
    serde_json::from_str(&raw).map_err(|e| {
        ApiError::bad(
            Code::Unexpected,
            format!("invalid introspection result: {e}"),
        )
    })
}

fn name_arg(args: &Value) -> ApiResult<String> {
    required_string(object(args, ARGS)?, "name", ARGS, "Text")
}

fn non_empty(obj: &Obj, key: &str) -> ApiResult<String> {
    let value = required_string(obj, key, ARGS, "Text")?;
    if value.is_empty() {
        return Err(parse_failed(
            &format!("{ARGS}.{key}"),
            "empty string not allowed",
        ));
    }
    Ok(value)
}

/// `add_remote_schema_permissions {remote_schema, role, definition, comment?}`. This
/// engine runs with remote schema permissions disabled (V2's default), where V2 refuses
/// the command.
pub async fn add_remote_schema_permissions<R: Runtime>(
    _ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let obj = object(args, ARGS)?;
    non_empty(obj, "remote_schema")?;
    non_empty(obj, "role")?;
    if field(obj, "definition").is_none() {
        return Err(missing(ARGS, "definition"));
    }
    Err(ApiError::bad(
        Code::ConstraintViolation,
        "remote schema permissions can only be added when remote schema permissions are enabled in the graphql-engine",
    ))
}

/// `drop_remote_schema_permissions {remote_schema, role}`.
pub async fn drop_remote_schema_permissions<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let obj = object(args, ARGS)?;
    let remote = non_empty(obj, "remote_schema")?;
    let role = non_empty(obj, "role")?;
    if !is_consistent(ctx, &remote) {
        return Err(not_exists(format!(
            "remote schema \"{remote}\" doesn't exist"
        )));
    }
    let i = index(&ctx.metadata.remote_schemas, &remote).expect("consistent remote");
    let Value::Object(entry) = &mut ctx.metadata.remote_schemas[i] else {
        return Err(not_exists(format!(
            "remote schema \"{remote}\" doesn't exist"
        )));
    };
    let role_of = |p: &Value| p.get("role").and_then(Value::as_str) == Some(role.as_str());
    let mut permissions = match entry.remove("permissions") {
        Some(Value::Array(p)) => p,
        _ => vec![],
    };
    let before = permissions.len();
    permissions.retain(|p| !role_of(p));
    let found = permissions.len() != before;
    if !permissions.is_empty() {
        entry.insert("permissions".into(), Value::Array(permissions));
    }
    if !found {
        return Err(not_exists(format!(
            "permissions for role: \"{role}\" for remote schema:\"{remote}\" doesn't exist"
        )));
    }
    ctx.check(Check::NoNew).await?;
    Ok(success())
}

/// Remove an inconsistent `remote_schema` object (`drop_inconsistent_metadata`).
pub(crate) fn drop_inconsistent(md: &mut hge_metadata::Metadata, obj: &InconsistentObject) -> bool {
    let Some(name) = obj.name.strip_prefix("remote_schema ") else {
        return false;
    };
    let name = name.to_owned();
    drop_dependents(md, &name);
    md.remote_schemas
        .retain(|e| entry_name(e) != Some(name.as_str()));
    true
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn metadata() -> hge_metadata::Metadata {
        hge_metadata::from_json(json!({
            "version": 3,
            "sources": [{
                "name": "default",
                "kind": "postgres",
                "configuration": {"connection_info": {"database_url": "postgres://x"}},
                "tables": [{
                    "table": {"schema": "custom", "name": "profiles"},
                    "remote_relationships": [
                        {"name": "messages", "definition": {"to_remote_schema": {
                            "remote_schema": "r", "lhs_fields": ["id"],
                            "remote_field": {"messages": {"arguments": {}}}}}},
                        {"name": "legacy", "definition": {"remote_schema": "r",
                            "hasura_fields": ["id"], "remote_field": {"m": {"arguments": {}}}}},
                        {"name": "other", "definition": {"to_remote_schema": {
                            "remote_schema": "s", "lhs_fields": ["id"],
                            "remote_field": {"m": {"arguments": {}}}}}}
                    ]
                }]
            }],
            "remote_schemas": [
                {"name": "r", "definition": {"url": "http://r"}},
                {"name": "s", "definition": {"url": "http://s"}, "remote_relationships": [
                    {"type_name": "User", "relationships": [
                        {"name": "toR", "definition": {"to_remote_schema": {
                            "remote_schema": "r", "lhs_fields": ["id"],
                            "remote_field": {"u": {"arguments": {}}}}}}
                    ]}
                ]}
            ]
        }))
        .unwrap()
    }

    #[test]
    fn dependents_in_both_formats_and_other_remote_schemas() {
        let md = metadata();
        let reports: Vec<String> = dependents(&md, "r").iter().map(Dependent::report).collect();
        assert_eq!(
            reports,
            [
                "remote relationship custom.profiles.legacy in source \"default\"",
                "remote relationship custom.profiles.messages in source \"default\"",
                "remote_schema_remote_relationship s.User.toR",
            ]
        );
    }

    #[test]
    fn cascade_drops_only_relationships_to_the_remote() {
        let mut md = metadata();
        drop_dependents(&mut md, "r");
        let names: Vec<&str> = md.sources[0].tables[0]
            .remote_relationships
            .iter()
            .filter_map(entry_name)
            .collect();
        assert_eq!(names, ["other"]);
        assert!(md.remote_schemas[1].get("remote_relationships").is_none());
        assert!(dependents(&md, "r").is_empty());
    }
}
