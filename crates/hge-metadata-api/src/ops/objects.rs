//! Backend-agnostic metadata objects: query collections and the allowlist, REST endpoints,
//! inherited roles, and server-level settings (API limits, metrics, the TLS allowlist,
//! introspection options, query tags, OpenTelemetry, data connector agents).
//!
//! Commands parse their arguments, check what V2 checks against the metadata itself
//! (existence, duplicates, dependents), edit the metadata and call [`Ctx::check`], so
//! `export_metadata` matches V2 after every command. Whether the result is consistent is
//! decided by the resolver, like for every other object: REST endpoints in `hge_rest`
//! (owned by the Allowlists/REST thread), OpenTelemetry in `hge_metadata::consistency`,
//! inherited roles with the permissions in hge-schema.
//!
//! Known gaps against V2: queries of allowlisted collections are not validated against the
//! GraphQL schema, and `dc_add_agent` only reproduces V2's answer for an unreachable agent.

use hge_core::Code;
use hge_metadata::{
    AllowlistEntry, AllowlistScope, ApiLimits, CollectionDefinition, EndpointDefinition,
    EndpointMethod, InheritedRole, ListedQuery, QueryCollection, QueryReference, RestEndpoint,
};
use serde_json::{Map, Value, json};

use super::parse::{self, Obj, array, dquote_list, duplicates, object, record, text};
use crate::api::json_kind;
use crate::ctx::{Check, Ctx};
use crate::response::{ApiError, ApiResult, already_exists, not_exists, parse_failed, success};
use crate::runtime::Runtime;

/// Commands this module implements.
pub const COMMANDS: &[&str] = &[
    "create_query_collection",
    "rename_query_collection",
    "drop_query_collection",
    "add_query_to_collection",
    "drop_query_from_collection",
    "add_collection_to_allowlist",
    "drop_collection_from_allowlist",
    "update_scope_of_collection_in_allowlist",
    "create_rest_endpoint",
    "drop_rest_endpoint",
    "set_api_limits",
    "remove_api_limits",
    "set_metrics_config",
    "remove_metrics_config",
    "add_inherited_role",
    "drop_inherited_role",
    "add_host_to_tls_allowlist",
    "drop_host_from_tls_allowlist",
    "set_graphql_schema_introspection_options",
    "set_query_tags",
    "set_opentelemetry_config",
    "set_opentelemetry_status",
    "dc_add_agent",
    "dc_delete_agent",
];

pub async fn run<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    command: &str,
    args: &Value,
) -> ApiResult<Value> {
    match command {
        "create_query_collection" => create_query_collection(ctx, args).await,
        "rename_query_collection" => rename_query_collection(ctx, args).await,
        "drop_query_collection" => drop_query_collection(ctx, args).await,
        "add_query_to_collection" => add_query_to_collection(ctx, args).await,
        "drop_query_from_collection" => drop_query_from_collection(ctx, args).await,
        "add_collection_to_allowlist" => add_collection_to_allowlist(ctx, args).await,
        "drop_collection_from_allowlist" => drop_collection_from_allowlist(ctx, args).await,
        "update_scope_of_collection_in_allowlist" => update_allowlist_scope(ctx, args).await,
        "create_rest_endpoint" => create_rest_endpoint(ctx, args).await,
        "drop_rest_endpoint" => drop_rest_endpoint(ctx, args).await,
        "set_api_limits" => set_api_limits(ctx, args).await,
        "remove_api_limits" => {
            ctx.metadata.api_limits = None;
            ctx.check(Check::Newly).await?;
            Ok(success())
        }
        "set_metrics_config" => set_metrics_config(ctx, args).await,
        "remove_metrics_config" => {
            ctx.metadata.metrics_config = None;
            ctx.check(Check::Newly).await?;
            Ok(success())
        }
        "add_inherited_role" => add_inherited_role(ctx, args).await,
        "drop_inherited_role" => drop_inherited_role(ctx, args).await,
        "add_host_to_tls_allowlist" => add_tls_host(ctx, args).await,
        "drop_host_from_tls_allowlist" => drop_tls_host(ctx, args).await,
        "set_graphql_schema_introspection_options" => set_introspection(ctx, args).await,
        "set_query_tags" => set_query_tags(ctx, args).await,
        "set_opentelemetry_config" => set_opentelemetry_config(ctx, args).await,
        "set_opentelemetry_status" => set_opentelemetry_status(ctx, args).await,
        // Normally answered before the metadata lock is taken (see `api::handle`).
        "dc_add_agent" => dc_add_agent(args).await,
        "dc_delete_agent" => dc_delete_agent(ctx, args).await,
        other => Err(super::not_implemented(other)),
    }
}

/// A non-empty text (V2's `NonEmptyText`).
fn non_empty(o: &Obj<'_>, key: &str) -> ApiResult<String> {
    let s = o.string(key)?;
    if s.is_empty() {
        return Err(parse_failed(&o.sub(key), "empty string not allowed"));
    }
    Ok(s)
}

fn texts(items: &[Value], path: &str) -> ApiResult<Vec<String>> {
    items
        .iter()
        .enumerate()
        .map(|(i, r)| text(r, &format!("{path}[{i}]")))
        .collect()
}

// ---------------------------------------------------------------------------------------
// Query collections and the allowlist

const QC: &str = "Hasura.RQL.Types.QueryCollection";

fn collection_index<R: Runtime>(ctx: &Ctx<'_, R>, name: &str) -> Option<usize> {
    ctx.metadata
        .query_collections
        .iter()
        .position(|c| c.name == name)
}

fn collection_not_exists(name: &str) -> ApiError {
    not_exists(format!(
        "query collection with name \"{name}\" does not exist"
    ))
}

/// One `{name, query}` entry; the query must parse as a GraphQL document.
fn listed_query(v: &Value, path: &str) -> ApiResult<ListedQuery> {
    let o = record(v, path, &format!("{QC}.ListedQuery(ListedQuery)"))?;
    let name = non_empty(&o, "name")?;
    let query = query_text(o.req("query")?, &o.sub("query"))?;
    Ok(ListedQuery { name, query })
}

fn query_text(v: &Value, path: &str) -> ApiResult<String> {
    let Value::String(q) = v else {
        return Err(parse_failed(path, "expecting String for GraphQL query"));
    };
    if async_graphql_parser::parse_query(q).is_err() {
        return Err(parse_failed(path, "parsing the graphql query failed"));
    }
    Ok(q.clone())
}

async fn create_query_collection<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let o = record(
        args,
        "$.args",
        &format!("{QC}.CreateCollection(CreateCollection)"),
    )?;
    let name = non_empty(&o, "name")?;
    let def = record(
        o.req("definition")?,
        &o.sub("definition"),
        &format!("{QC}.CollectionDef(CollectionDef)"),
    )?;
    let queries = def.array("queries")?;
    let queries = queries
        .iter()
        .enumerate()
        .map(|(i, q)| listed_query(q, &format!("{}[{i}]", def.sub("queries"))))
        .collect::<ApiResult<Vec<_>>>()?;
    let comment = o.opt_string("comment")?;
    if collection_index(ctx, &name).is_some() {
        return Err(already_exists(format!(
            "query collection with name \"{name}\" already exists"
        ))
        .with_path("$.args.name"));
    }
    let dups = duplicates(queries.iter().map(|q| q.name.as_str()));
    if !dups.is_empty() {
        return Err(ApiError::bad(
            Code::NotSupported,
            format!("found duplicate query names {}", dquote_list(&dups)),
        )
        .with_path("$.args.definition.queries"));
    }
    ctx.metadata.query_collections.push(QueryCollection {
        name,
        definition: CollectionDefinition { queries },
        comment,
    });
    ctx.check(Check::Newly).await?;
    Ok(success())
}

async fn rename_query_collection<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let o = record(
        args,
        "$.args",
        &format!("{QC}.RenameCollection(RenameCollection)"),
    )?;
    let name = non_empty(&o, "name")?;
    let new_name = non_empty(&o, "new_name")?;
    let idx = collection_index(ctx, &name).ok_or_else(|| collection_not_exists(&name))?;
    if collection_index(ctx, &new_name).is_some() {
        return Err(already_exists(format!(
            "query collection with name \"{new_name}\" already exists"
        ))
        .with_path("$.args.new_name"));
    }
    let mut entry = ctx.metadata.query_collections.remove(idx);
    entry.name = new_name;
    ctx.metadata.query_collections.push(entry);
    ctx.check(Check::Newly).await?;
    Ok(success())
}

fn allowlist_index<R: Runtime>(ctx: &Ctx<'_, R>, name: &str) -> Option<usize> {
    ctx.metadata
        .allowlist
        .iter()
        .position(|e| e.collection == name)
}

async fn drop_query_collection<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let o = record(
        args,
        "$.args",
        &format!("{QC}.DropCollection(DropCollection)"),
    )?;
    let name = non_empty(&o, "collection")?;
    let cascade = o.req_bool("cascade")?;
    let idx = collection_index(ctx, &name)
        .ok_or_else(|| collection_not_exists(&name).with_path("$.args.collection"))?;
    let in_allowlist = allowlist_index(ctx, &name);
    if in_allowlist.is_some() && !cascade {
        return Err(ApiError::bad(
            Code::DependencyError,
            format!(
                "query collection with name \"{name}\" is present in the allowlist; cannot proceed to drop. please use cascade to confirm you wish to drop it from the allowlist as well"
            ),
        )
        .with_path("$.args.collection"));
    }
    if let Some(a) = in_allowlist {
        ctx.metadata.allowlist.remove(a);
    }
    ctx.metadata.query_collections.remove(idx);
    ctx.check(Check::Newly).await?;
    Ok(success())
}

async fn add_query_to_collection<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let o = record(
        args,
        "$.args",
        &format!("{QC}.AddQueryToCollection(AddQueryToCollection)"),
    )?;
    let coll = non_empty(&o, "collection_name")?;
    let qname = non_empty(&o, "query_name")?;
    let query = query_text(o.req("query")?, &o.sub("query"))?;
    let idx = collection_index(ctx, &coll).ok_or_else(|| collection_not_exists(&coll))?;
    if ctx.metadata.query_collections[idx].query(&qname).is_some() {
        return Err(already_exists(format!(
            "query with name \"{qname}\" already exists in collection \"{coll}\""
        )));
    }
    ctx.metadata.query_collections[idx]
        .definition
        .queries
        .push(ListedQuery { name: qname, query });
    ctx.check(Check::Newly).await?;
    Ok(success())
}

async fn drop_query_from_collection<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let o = record(
        args,
        "$.args",
        &format!("{QC}.DropQueryFromCollection(DropQueryFromCollection)"),
    )?;
    let coll = non_empty(&o, "collection_name")?;
    let qname = non_empty(&o, "query_name")?;
    let idx = collection_index(ctx, &coll).ok_or_else(|| collection_not_exists(&coll))?;
    if ctx.metadata.query_collections[idx].query(&qname).is_none() {
        return Err(ApiError::bad(
            Code::NotFound,
            format!("query with name \"{qname}\" not found in collection \"{coll}\""),
        ));
    }
    ctx.metadata.query_collections[idx]
        .definition
        .queries
        .retain(|q| q.name != qname);
    ctx.check(Check::Newly).await?;
    Ok(success())
}

/// `scope`: `{global: true}` or `{global: false, roles: [...]}`.
fn allowlist_scope(v: &Value, path: &str) -> ApiResult<AllowlistScope> {
    let o = object(v, path, "AllowlistScope")?;
    let global = o.req_bool("global")?;
    if global {
        if o.get("roles").is_some() {
            return Err(parse_failed(
                path,
                "roles are not allowed when global is true",
            ));
        }
        return Ok(AllowlistScope::Global);
    }
    let roles = o.array("roles")?;
    if roles.is_empty() {
        return Err(parse_failed(
            &o.sub("roles"),
            "parsing NonEmpty failed, unexpected empty list",
        ));
    }
    let roles = texts(roles, &o.sub("roles"))?;
    if !duplicates(roles.iter().map(String::as_str)).is_empty() {
        return Err(parse_failed(path, "duplicate roles are not allowed"));
    }
    Ok(AllowlistScope::Roles(roles))
}

fn not_in_allowlist(coll: &str) -> ApiError {
    ApiError::bad(
        Code::NotFound,
        format!("collection \"{coll}\" doesn't exist in the allowlist"),
    )
}

async fn add_collection_to_allowlist<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let o = object(args, "$.args", "AllowlistEntry")?;
    let coll = non_empty(&o, "collection")?;
    let scope = match o.get("scope") {
        Some(s) => allowlist_scope(s, &o.sub("scope"))?,
        None => AllowlistScope::Global,
    };
    if collection_index(ctx, &coll).is_none() {
        return Err(collection_not_exists(&coll).with_path("$.args.collection"));
    }
    if allowlist_index(ctx, &coll).is_some() {
        return Ok(json!({"message": format!(
            "collection \"{coll}\" already exists in the allowlist, scope ignored; to change scope, use update_scope_of_collection_in_allowlist"
        )}));
    }
    ctx.metadata.allowlist.push(AllowlistEntry {
        collection: coll,
        scope,
    });
    ctx.check(Check::Newly).await?;
    Ok(success())
}

async fn drop_collection_from_allowlist<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let o = record(
        args,
        "$.args",
        "Hasura.RQL.Types.Allowlist.DropCollectionFromAllowlist(DropCollectionFromAllowlist)",
    )?;
    let coll = non_empty(&o, "collection")?;
    if collection_index(ctx, &coll).is_none() {
        return Err(collection_not_exists(&coll).with_path("$.args.collection"));
    }
    let idx = allowlist_index(ctx, &coll).ok_or_else(|| not_in_allowlist(&coll))?;
    ctx.metadata.allowlist.remove(idx);
    ctx.check(Check::Newly).await?;
    Ok(success())
}

async fn update_allowlist_scope<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let o = object(args, "$.args", "UpdateScopeOfCollectionInAllowlist")?;
    let coll = non_empty(&o, "collection")?;
    let scope = allowlist_scope(o.req("scope")?, &o.sub("scope"))?;
    if collection_index(ctx, &coll).is_none() {
        return Err(collection_not_exists(&coll).with_path("$.args.collection"));
    }
    let idx = allowlist_index(ctx, &coll).ok_or_else(|| not_in_allowlist(&coll))?;
    ctx.metadata.allowlist[idx] = AllowlistEntry {
        collection: coll,
        scope,
    };
    ctx.check(Check::Newly).await?;
    Ok(success())
}

// ---------------------------------------------------------------------------------------
// REST endpoints

async fn create_rest_endpoint<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let o = object(args, "$.args", "EndpointMetadata")?;
    // The endpoint is decoded with its autodocodec codec.
    for key in ["name", "url", "methods", "definition"] {
        if o.get(key).is_none() {
            return Err(parse_failed(
                "$.args",
                format!("the key '{key}' was not present"),
            ));
        }
    }
    let name = non_empty(&o, "name")?;
    let url = non_empty(&o, "url")?;
    let methods_v = o.array("methods")?;
    if methods_v.is_empty() {
        return Err(parse_failed(
            &o.sub("methods"),
            "parsing NonEmpty failed, unexpected empty list",
        ));
    }
    let mut methods = vec![];
    for (i, m) in methods_v.iter().enumerate() {
        let p = format!("{}[{i}]", o.sub("methods"));
        let s = text(m, &p)?;
        let Some(method) = EndpointMethod::parse(&s) else {
            return Err(parse_failed(
                &p,
                format!(
                    "parsing Hasura.RQL.Types.Endpoint.EndpointMethod failed, expected one of the tags [\"GET\",\"POST\",\"PUT\",\"DELETE\",\"PATCH\"], but found tag \"{s}\""
                ),
            ));
        };
        methods.push(method);
    }
    let def = object(o.req("definition")?, &o.sub("definition"), "EndpointDef")?;
    let q = object(def.req("query")?, &def.sub("query"), "QueryReference")?;
    let collection_name = non_empty(&q, "collection_name")?;
    let query_name = non_empty(&q, "query_name")?;
    let comment = o.opt_string("comment")?;
    if ctx.metadata.rest_endpoints.iter().any(|e| e.name == name) {
        return Err(already_exists(format!(
            "Endpoint with name: {name} already exists"
        )));
    }
    let id = format!("rest_endpoint {name}");
    ctx.metadata.rest_endpoints.push(RestEndpoint {
        name,
        url,
        methods,
        definition: EndpointDefinition {
            query: QueryReference {
                collection_name,
                query_name,
            },
        },
        comment,
    });
    ctx.check(Check::object("rest_endpoint", id)).await?;
    Ok(success())
}

async fn drop_rest_endpoint<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let o = object(args, "$.args", "DropEndpoint")?;
    let name = non_empty(&o, "name")?;
    let Some(idx) = ctx
        .metadata
        .rest_endpoints
        .iter()
        .position(|e| e.name == name)
    else {
        return Err(not_exists(format!(
            "endpoint with name: {name} does not exist"
        )));
    };
    ctx.metadata.rest_endpoints.remove(idx);
    ctx.check(Check::Newly).await?;
    Ok(success())
}

// ---------------------------------------------------------------------------------------
// API limits and metrics

fn number(v: &Value, path: &str, float: bool) -> ApiResult<Value> {
    match v {
        // aeson encodes a whole number of seconds without a fraction.
        Value::Number(n) if float => Ok(match n.as_f64() {
            Some(f) if !n.is_i64() && !n.is_u64() && f.fract() == 0.0 && f.abs() < 1e15 => {
                Value::from(f as i64)
            }
            _ => v.clone(),
        }),
        Value::Number(_) => parse::int(v, path).map(Value::from),
        other => Err(parse_failed(
            path,
            format!(
                "parsing {} failed, expected Number, but encountered {}",
                if float { "Double" } else { "Int" },
                json_kind(other)
            ),
        )),
    }
}

fn int_value(v: &Value, path: &str) -> ApiResult<Value> {
    number(v, path, false)
}

fn seconds_value(v: &Value, path: &str) -> ApiResult<Value> {
    number(v, path, true)
}

fn unique_params(v: &Value, path: &str) -> ApiResult<Value> {
    let err = || {
        parse_failed(
            path,
            "Not a valid value. Should be either: 'IP' or a list of Hasura session variables",
        )
    };
    match v {
        Value::String(s) if s.eq_ignore_ascii_case("ip") => Ok(json!("IP")),
        Value::Array(items) => {
            for i in items {
                let ok = i.as_str().is_some_and(|s| {
                    let l = s.to_ascii_lowercase();
                    l.starts_with("x-hasura-") && l != "x-hasura-role"
                });
                if !ok {
                    return Err(err());
                }
            }
            Ok(v.clone())
        }
        _ => Err(err()),
    }
}

fn rate_value(v: &Value, path: &str) -> ApiResult<Value> {
    let o = record(v, path, "RateLimitConfig")?;
    let max = int_value(o.req("max_reqs_per_min")?, &o.sub("max_reqs_per_min"))?;
    let up = match o.get("unique_params") {
        Some(u) => unique_params(u, &o.sub("unique_params"))?,
        None => Value::Null,
    };
    Ok(json!({"max_reqs_per_min": max, "unique_params": up}))
}

/// `{global, per_role}` with each value checked by `value`.
fn limit(v: &Value, path: &str, value: fn(&Value, &str) -> ApiResult<Value>) -> ApiResult<Value> {
    let o = object(v, path, "Limit")?;
    let global = value(o.req("global")?, &o.sub("global"))?;
    let mut per_role = Map::new();
    if let Some(p) = o.get("per_role") {
        let pr = object(p, &o.sub("per_role"), "Map")?;
        let mut keys: Vec<&String> = pr.map.keys().collect();
        keys.sort();
        for k in keys {
            per_role.insert(k.clone(), value(&pr.map[k], &pr.sub(k))?);
        }
    }
    Ok(json!({"global": global, "per_role": per_role}))
}

/// Parses the limit value of one kind of API limit.
type LimitParser = fn(&Value, &str) -> ApiResult<Value>;

async fn set_api_limits<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let o = object(args, "$.args", "ApiLimit")?;
    let mut out = Map::new();
    let kinds: [(&str, LimitParser); 5] = [
        ("batch_limit", int_value),
        ("depth_limit", int_value),
        ("node_limit", int_value),
        ("rate_limit", rate_value),
        ("time_limit", seconds_value),
    ];
    let mut parsed = vec![];
    for (key, f) in kinds {
        if let Some(v) = o.get(key) {
            parsed.push((key, limit(v, &o.sub(key), f)?));
        }
    }
    let disabled = o.bool_or("disabled", false)?;
    let empty = parsed.is_empty() && !disabled;
    for (key, v) in parsed {
        out.insert(key.into(), v);
    }
    out.insert("disabled".into(), Value::Bool(disabled));
    // The values were checked (and normalised) above, with V2's paths.
    ctx.metadata.api_limits = match empty {
        true => None,
        false => Some(
            serde_json::from_value::<ApiLimits>(Value::Object(out))
                .map_err(|e| parse_failed("$.args", e.to_string()))?,
        ),
    };
    ctx.check(Check::Newly).await?;
    Ok(success())
}

async fn set_metrics_config<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let o = object(args, "$.args", "MetricsConfig")?;
    let flag = |k: &str| -> ApiResult<bool> {
        match o.req(k)? {
            Value::Bool(b) => Ok(*b),
            other => Err(parse_failed(
                &o.sub(k),
                format!("expected Bool, but encountered {}", json_kind(other)),
            )),
        }
    };
    let vars = flag("analyze_query_variables")?;
    let body = flag("analyze_response_body")?;
    ctx.metadata.metrics_config = (vars || body)
        .then(|| json!({"analyze_query_variables": vars, "analyze_response_body": body}));
    ctx.check(Check::Newly).await?;
    Ok(success())
}

// ---------------------------------------------------------------------------------------
// Inherited roles

async fn add_inherited_role<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let o = object(args, "$.args", "InheritedRole")?;
    let role_name = non_empty(&o, "role_name")?;
    let mut role_set = texts(o.array("role_set")?, &o.sub("role_set"))?;
    if let Some(i) = role_set.iter().position(String::is_empty) {
        return Err(parse_failed(
            &format!("{}[{i}]", o.sub("role_set")),
            "empty string not allowed",
        ));
    }
    role_set.sort();
    role_set.dedup();
    if role_set.contains(&role_name) {
        return Err(ApiError::bad(
            Code::InvalidParams,
            "an inherited role name cannot be in the role combination",
        ));
    }
    let role = InheritedRole {
        role_name: role_name.clone(),
        role_set,
        extra: Map::new(),
    };
    match ctx
        .metadata
        .inherited_roles
        .iter_mut()
        .find(|r| r.role_name == role_name)
    {
        Some(r) => *r = role,
        None => ctx.metadata.inherited_roles.push(role),
    }
    let id = format!("inherited_role inherited role {role_name}");
    ctx.check(Check::object("inherited_role", id)).await?;
    Ok(success())
}

async fn drop_inherited_role<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let o = object(args, "$.args", "DropInheritedRole")?;
    let role_name = non_empty(&o, "role_name")?;
    let Some(idx) = ctx
        .metadata
        .inherited_roles
        .iter()
        .position(|r| r.role_name == role_name)
    else {
        return Err(not_exists(format!(
            "\"{role_name}\" inherited role doesn't exist"
        )));
    };
    ctx.metadata.inherited_roles.remove(idx);
    ctx.check(Check::NoNew).await?;
    Ok(success())
}

// ---------------------------------------------------------------------------------------
// Network: TLS allowlist

fn tls_list<R: Runtime>(ctx: &Ctx<'_, R>) -> Vec<Value> {
    ctx.metadata
        .network
        .as_ref()
        .and_then(|n| n.get("tls_allowlist"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn set_tls_list<R: Runtime>(ctx: &mut Ctx<'_, R>, list: Vec<Value>) {
    let mut network = match ctx.metadata.network.take() {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    };
    if list.is_empty() {
        network.remove("tls_allowlist");
    } else {
        network.insert("tls_allowlist".into(), Value::Array(list));
    }
    ctx.metadata.network = (!network.is_empty()).then_some(Value::Object(network));
}

fn tls_matches(entry: &Value, host: &str, suffix: Option<&str>) -> bool {
    entry.get("host").and_then(Value::as_str) == Some(host)
        && entry.get("suffix").and_then(Value::as_str) == suffix
}

async fn add_tls_host<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let o = object(args, "$.args", "TlsAllow")?;
    let host = o.string("host")?;
    let suffix = o.opt_string("suffix")?;
    let permissions = match o.opt_array("permissions")? {
        Some(p) => {
            if let Some(i) = p.iter().position(|v| v != "self-signed") {
                return Err(parse_failed(
                    &format!("{}[{i}]", o.sub("permissions")),
                    "TlsPermission expecting one of SelfSigned",
                ));
            }
            Value::Array(p.clone())
        }
        None => Value::Null,
    };
    if host.is_empty() {
        return Err(ApiError::bad(
            Code::BadRequest,
            "key \"host\" cannot be empty",
        ));
    }
    let mut list = tls_list(ctx);
    if list
        .iter()
        .any(|e| tls_matches(e, &host, suffix.as_deref()))
    {
        return Err(already_exists(match &suffix {
            None => format!("the host \"{host}\" already exists in the allowlist"),
            Some(s) => {
                format!("the host \"{host}\" with suffix \"{s}\" already exists in the allowlist")
            }
        }));
    }
    list.push(json!({"host": host, "permissions": permissions, "suffix": suffix}));
    set_tls_list(ctx, list);
    ctx.check(Check::Newly).await?;
    Ok(success())
}

async fn drop_tls_host<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let o = object(args, "$.args", "DropHostFromTLSAllowlist")?;
    let host = o.string("host")?;
    let suffix = o.opt_string("suffix")?;
    if host.is_empty() {
        return Err(ApiError::bad(Code::BadRequest, "hostname cannot be empty"));
    }
    let mut list = tls_list(ctx);
    if !list
        .iter()
        .any(|e| tls_matches(e, &host, suffix.as_deref()))
    {
        return Err(not_exists(match &suffix {
            None => format!("the host \"{host}\" isn't present in the allowlist"),
            Some(s) => {
                format!("the host \"{host}\" with suffix \"{s}\" isn't present in the allowlist")
            }
        }));
    }
    list.retain(|e| !tls_matches(e, &host, suffix.as_deref()));
    set_tls_list(ctx, list);
    ctx.check(Check::Newly).await?;
    Ok(success())
}

// ---------------------------------------------------------------------------------------
// Introspection options, query tags

async fn set_introspection<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let o = record(
        args,
        "$.args",
        "Hasura.RQL.Types.GraphqlSchemaIntrospection.SetGraphqlIntrospectionOptions(SetGraphqlIntrospectionOptions)",
    )?;
    let mut roles = texts(o.array("disabled_for_roles")?, &o.sub("disabled_for_roles"))?;
    roles.sort();
    roles.dedup();
    ctx.metadata.graphql_schema_introspection =
        (!roles.is_empty()).then(|| json!({ "disabled_for_roles": roles }));
    ctx.check(Check::Newly).await?;
    Ok(success())
}

async fn set_query_tags<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let o = object(args, "$.args", "SetQueryTagsConfig")?;
    let source = o.string("source_name")?;
    // V2 reads the configuration from the top level of the arguments.
    let disabled = o.bool_or("disabled", false)?;
    let format = match o.get("format") {
        None => "standard".to_owned(),
        Some(v) => {
            let s = text(v, &o.sub("format"))?;
            if !matches!(
                s.as_str(),
                "standard" | "sqlcommenter" | "standard_prepended"
            ) {
                return Err(parse_failed(
                    &o.sub("format"),
                    "Not a valid query tags format value. Use either standard, sqlcommenter, or standard_prepended",
                ));
            }
            s
        }
    };
    let omit = o.bool_or("omit_request_id", true)?;
    if ctx.metadata.source(&source).is_none() {
        return Err(not_exists(format!(
            "source with name \"{source}\" does not exist"
        )));
    }
    if let Some(s) = ctx.metadata.source_mut(&source) {
        s.extra.insert(
            "query_tags".into(),
            json!({"disabled": disabled, "format": format, "omit_request_id": omit}),
        );
    }
    ctx.check(Check::NoNew).await?;
    Ok(success())
}

// ---------------------------------------------------------------------------------------
// OpenTelemetry

fn otel_status(v: &Value, path: &str) -> ApiResult<String> {
    let bad = |shown: &str| {
        parse_failed(
            path,
            format!("OpenTelemetry status must be either \"enabled\" or \"disabled\", got {shown}"),
        )
    };
    match v {
        Value::String(s) if s == "enabled" || s == "disabled" => Ok(s.clone()),
        Value::String(s) if s.starts_with("{{") && s.ends_with("}}") => Ok(s.clone()),
        Value::String(s) => Err(bad(s)),
        other => Err(parse_failed(
            path,
            format!(
                "parsing OtelStatusConfig failed, expected String, but encountered {}",
                json_kind(other)
            ),
        )),
    }
}

fn enum_list(v: &Value, path: &str, allowed: &[&str], set: bool) -> ApiResult<Vec<String>> {
    let items = texts(array(v, path)?, path)?;
    for (i, s) in items.iter().enumerate() {
        if !allowed.contains(&s.as_str()) {
            return Err(parse_failed(
                &format!("{path}[{i}]"),
                format!("unexpected string '\"{s}\"'."),
            ));
        }
    }
    let mut items = items;
    if set {
        items.sort_by_key(|s| allowed.iter().position(|a| a == s));
        items.dedup();
    }
    Ok(items)
}

fn otel_exporter(v: &Value, path: &str) -> ApiResult<Value> {
    let o = object(v, path, "OtelExporterConfig")?;
    let mut out = Map::new();
    for key in [
        "otlp_traces_endpoint",
        "otlp_metrics_endpoint",
        "otlp_logs_endpoint",
    ] {
        if let Some(s) = o.opt_string(key)? {
            out.insert(key.into(), Value::String(s));
        }
    }
    if let Some(p) = o.get("protocol") {
        let s = text(p, &o.sub("protocol"))?;
        let err = match s.as_str() {
            "http/protobuf" => None,
            "http/json" => Some("http/json is not supported".to_owned()),
            "grpc" => Some("gRPC is not supported".to_owned()),
            _ => Some(format!("unexpected string '\"{s}\"'.")),
        };
        if let Some(e) = err {
            return Err(parse_failed(&o.sub("protocol"), e));
        }
    }
    out.insert("protocol".into(), json!("http/protobuf"));
    let headers = match o.get("headers") {
        Some(h) => parse::headers(h, &o.sub("headers"))?,
        None => vec![],
    };
    out.insert("headers".into(), Value::Array(headers));
    let attrs = match o.opt_array("resource_attributes")? {
        Some(a) => a
            .iter()
            .enumerate()
            .map(|(i, nv)| {
                let p = format!("{}[{i}]", o.sub("resource_attributes"));
                let nv = object(nv, &p, "name-value pair")?;
                Ok(json!({"name": nv.string("name")?, "value": nv.string("value")?}))
            })
            .collect::<ApiResult<Vec<_>>>()?,
        None => vec![],
    };
    out.insert("resource_attributes".into(), Value::Array(attrs));
    let props = match o.get("traces_propagators") {
        Some(p) => enum_list(
            p,
            &o.sub("traces_propagators"),
            &["b3", "tracecontext"],
            false,
        )?,
        None => vec!["b3".to_owned()],
    };
    out.insert("traces_propagators".into(), json!(props));
    Ok(hge_metadata::export::sorted(Value::Object(out)))
}

fn otel_batch(v: &Value, path: &str) -> ApiResult<Value> {
    let o = object(v, path, "OtelBatchSpanProcessorConfig")?;
    let size = o.opt_int("max_export_batch_size")?.unwrap_or(512);
    Ok(json!({ "max_export_batch_size": size }))
}

fn default_otel() -> Value {
    json!({
        "status": "disabled",
        "data_types": [],
        "exporter_otlp": otel_exporter(&json!({}), "$").unwrap_or(Value::Null),
        "batch_span_processor": {"max_export_batch_size": 512},
    })
}

/// Store an OpenTelemetry configuration, leaving it out when it is V2's empty configuration.
fn store_otel<R: Runtime>(ctx: &mut Ctx<'_, R>, config: Value) {
    ctx.metadata.opentelemetry = (config != default_otel()).then_some(config);
}

/// `drop_inconsistent_metadata` of an OpenTelemetry part (`exporter_otlp` or
/// `batch_span_processor`): V2 puts back its default.
pub(crate) fn reset_otel_part(md: &mut hge_metadata::Metadata, part: &str) {
    let Some(mut config) = md.opentelemetry.take() else {
        return;
    };
    if let Some(default) = default_otel().get(part) {
        config[part] = default.clone();
    }
    md.opentelemetry = (config != default_otel()).then_some(config);
}

async fn set_opentelemetry_config<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let o = object(args, "$.args", "OpenTelemetryConfig")?;
    let status = match o.get("status") {
        Some(s) => otel_status(s, &o.sub("status"))?,
        None => "disabled".to_owned(),
    };
    let data_types = match o.get("data_types") {
        Some(d) => enum_list(
            d,
            &o.sub("data_types"),
            &["traces", "metrics", "logs"],
            true,
        )?,
        None => vec![],
    };
    let empty = json!({});
    let exporter = otel_exporter(
        o.get("exporter_otlp").unwrap_or(&empty),
        &o.sub("exporter_otlp"),
    )?;
    let batch = otel_batch(
        o.get("batch_span_processor").unwrap_or(&empty),
        &o.sub("batch_span_processor"),
    )?;
    store_otel(
        ctx,
        json!({"status": status, "data_types": data_types, "exporter_otlp": exporter, "batch_span_processor": batch}),
    );
    ctx.check(Check::NoNew).await?;
    Ok(success())
}

async fn set_opentelemetry_status<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let status = otel_status(args, "$.args")?;
    let mut config = ctx
        .metadata
        .opentelemetry
        .clone()
        .unwrap_or_else(default_otel);
    config["status"] = Value::String(status);
    store_otel(ctx, config);
    ctx.check(Check::NoNew).await?;
    Ok(success())
}

// ---------------------------------------------------------------------------------------
// Data connector agents

/// Probe the agent. It is never added (data connector backends are not supported), so this
/// does not touch the metadata and runs without the metadata lock.
pub async fn dc_add_agent(args: &Value) -> ApiResult<Value> {
    let o = object(args, "$.args", "DCAddAgent")?;
    o.string("name")?;
    let url = o.string("url")?;
    let reachable = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(client) => client
            .get(format!("{}/capabilities", url.trim_end_matches('/')))
            .send()
            .await
            .is_ok(),
        Err(_) => false,
    };
    if !reachable {
        return Ok(json!({
            "details": {
                "code": "connection-not-established",
                "error": "Error communicating with data connector agent: Connection failure",
                "path": "$",
            },
            "message": "Agent is not available",
        }));
    }
    Err(super::not_implemented("dc_add_agent"))
}

async fn dc_delete_agent<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let o = object(args, "$.args", "DCDeleteAgent")?;
    let name = o.string("name")?;
    let known = ctx
        .metadata
        .backend_configs
        .as_ref()
        .and_then(|b| b.get("dataconnector"))
        .and_then(Value::as_object)
        .is_some_and(|a| a.contains_key(&name));
    if !known {
        return Err(ApiError::bad(
            Code::NotFound,
            format!("DC Agent '{name}' not found"),
        ));
    }
    if let Some(Value::Object(b)) = ctx.metadata.backend_configs.as_mut() {
        if let Some(Value::Object(dc)) = b.get_mut("dataconnector") {
            dc.remove(&name);
            if dc.is_empty() {
                b.remove("dataconnector");
            }
        }
        if b.is_empty() {
            ctx.metadata.backend_configs = None;
        }
    }
    ctx.check(Check::Newly).await?;
    Ok(success())
}
