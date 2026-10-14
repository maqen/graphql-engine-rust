//! Table remote relationships: `pg_create_remote_relationship`,
//! `pg_update_remote_relationship` and `pg_delete_remote_relationship` (spec 08 §8.2, V2's
//! `RQL/DDL/RemoteRelationship.hs`). The definition is stored as given (legacy or unified,
//! so it exports the same); it is validated when the metadata is resolved.

use hge_core::{Code, QualifiedTable};
use serde_json::{Map, Value};

use super::sources_tables::{Args, catalog, table_item_name, table_mut, table_ref};
use crate::ctx::{Check, Ctx};
use crate::response::{ApiError, ApiResult, not_exists, parse_failed, success};
use crate::runtime::Runtime;

/// Commands this module implements (`pg_` prefix stripped).
pub const COMMANDS: &[&str] = &[
    "create_remote_relationship",
    "update_remote_relationship",
    "delete_remote_relationship",
];

pub async fn run<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    command: &str,
    args: &Value,
) -> ApiResult<Value> {
    match command {
        "create_remote_relationship" => create(ctx, args, false).await,
        "update_remote_relationship" => create(ctx, args, true).await,
        "delete_remote_relationship" => delete(ctx, args).await,
        _ => Err(super::not_implemented(command)),
    }
}

const KIND: &str = "remote_relationship";

fn rel_name(v: &Value) -> Option<&str> {
    v.get("name").and_then(Value::as_str)
}

/// `{source?, table, name}` plus, for create and update, the metadata entry to store.
fn entry_args(a: &Args<'_>) -> ApiResult<Map<String, Value>> {
    let name = a.non_empty("name")?;
    let definition = match (a.raw("remote_schema"), a.raw("definition")) {
        // Legacy: the definition is inline (`remote_schema`, `hasura_fields`,
        // `remote_field`) and stays legacy in metadata.
        (Some(_), None) => {
            let mut def = Map::new();
            for key in ["remote_schema", "hasura_fields", "remote_field"] {
                let v = a.raw(key).ok_or_else(|| a.missing(key))?;
                def.insert(key.into(), v.clone());
            }
            Value::Object(def)
        }
        (None, Some(def)) => {
            let keys = ["to_source", "to_remote_schema"];
            let n = def
                .as_object()
                .map_or(0, |o| keys.iter().filter(|k| o.contains_key(**k)).count());
            if n != 1 {
                return Err(parse_failed(
                    &a.field_path("definition"),
                    "remote relationship definition (strict format) expects exactly one of: to_source, to_remote_schema",
                ));
            }
            def.clone()
        }
        _ => {
            return Err(parse_failed(
                "$.args",
                "create_remote_relationship expects exactly one of: remote_schema, definition",
            ));
        }
    };
    let mut entry = Map::new();
    entry.insert("name".into(), Value::String(name));
    entry.insert("definition".into(), definition);
    Ok(entry)
}

/// V2's `askRemoteRel`: the name must be a remote relationship in the schema cache (an
/// inconsistent one is not), and not another kind of field.
fn ask_remote_rel<R: Runtime>(
    ctx: &Ctx<'_, R>,
    source: &str,
    table: &QualifiedTable,
    name: &str,
) -> ApiResult<()> {
    let entry = table_ref(&ctx.metadata, source, table)?;
    let id = table_item_name(KIND, name, source, table);
    let consistent = entry
        .remote_relationships
        .iter()
        .any(|r| rel_name(r) == Some(name))
        && !ctx
            .inconsistencies
            .iter()
            .any(|o| o.kind == KIND && o.name == id);
    if consistent {
        return Ok(());
    }
    let named = |v: &Value| rel_name(v) == Some(name);
    let other_field = entry.object_relationships.iter().any(|r| r.name == name)
        || entry.array_relationships.iter().any(|r| r.name == name)
        || entry.computed_fields.iter().any(named)
        || catalog(ctx, source)
            .and_then(|c| c.table(table).map(|t| t.columns.contains_key(name)))
            .unwrap_or(false);
    if other_field {
        return Err(ApiError::bad(
            Code::UnexpectedPayload,
            "expecting a remote relationship",
        ));
    }
    Err(not_exists(format!("\"{name}\" does not exist")))
}

async fn create<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value, update: bool) -> ApiResult<Value> {
    let a = Args::new(args)?;
    let source = a.source()?;
    let table = a.table("table")?;
    let entry = entry_args(&a)?;
    let name = rel_name(&Value::Object(entry.clone()))
        .unwrap_or_default()
        .to_owned();
    if update {
        ask_remote_rel(ctx, &source, &table, &name)?;
    }
    // Create overwrites a relationship of the same name, like V2.
    let t = table_mut(&mut ctx.metadata, &source, &table)?;
    let entry = Value::Object(entry);
    match t
        .remote_relationships
        .iter_mut()
        .find(|r| rel_name(r) == Some(&name))
    {
        Some(existing) => *existing = entry,
        None => t.remote_relationships.push(entry),
    }
    ctx.check(Check::object(
        KIND,
        table_item_name(KIND, &name, &source, &table),
    ))
    .await?;
    Ok(success())
}

async fn delete<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let a = Args::new(args)?;
    let source = a.source()?;
    let table = a.table("table")?;
    let name = a.non_empty("name")?;
    ask_remote_rel(ctx, &source, &table, &name)?;
    let t = table_mut(&mut ctx.metadata, &source, &table)?;
    t.remote_relationships
        .retain(|r| rel_name(r) != Some(&name));
    ctx.check(Check::NoNew).await?;
    Ok(success())
}
