//! Postgres functions, computed fields, logical models and native queries (`pg_` commands).
//!
//! Commands parse their arguments as V2 does, check what V2 checks against the metadata
//! (already tracked, not found, dependents), edit the metadata and call [`Ctx::check`].
//! Whether the objects resolve against the database (function signatures, computed field
//! arguments, native query code, logical model references) is the resolver's job, owned by
//! the Mutations thread; until it checks them, such changes are accepted.
//!
//! Known gaps until then: inconsistent logical models and native queries are not reported.

use hge_core::{Code, QualifiedTable};
use hge_metadata::Source;
use serde_json::{Map, Value, json};

use crate::api::json_kind;
use crate::ctx::{Check, Ctx};
use crate::ops::parse::{Obj, object, qualified, qualified_json, record, text};
use crate::response::{ApiError, ApiResult, already_exists, not_exists, parse_failed, success};
use crate::runtime::Runtime;
use crate::schema_diff::{Rels, bool_exp_mentions};

/// Commands this module implements (`pg_` prefix stripped for backend commands).
pub const COMMANDS: &[&str] = &[
    "track_function",
    "untrack_function",
    "set_function_customization",
    "create_function_permission",
    "drop_function_permission",
    "add_computed_field",
    "drop_computed_field",
    "track_native_query",
    "untrack_native_query",
    "get_native_query",
    "track_logical_model",
    "untrack_logical_model",
    "get_logical_model",
    "create_logical_model_select_permission",
    "drop_logical_model_select_permission",
];

pub async fn run<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    command: &str,
    args: &Value,
) -> ApiResult<Value> {
    match command {
        "track_function" => track_function(ctx, args).await,
        "untrack_function" => untrack_function(ctx, args).await,
        "set_function_customization" => set_function_customization(ctx, args).await,
        "create_function_permission" => create_function_permission(ctx, args).await,
        "drop_function_permission" => drop_function_permission(ctx, args).await,
        "add_computed_field" => add_computed_field(ctx, args).await,
        "drop_computed_field" => drop_computed_field(ctx, args).await,
        "track_native_query" => track_native_query(ctx, args).await,
        "untrack_native_query" => untrack_native_query(ctx, args).await,
        "get_native_query" => get_native_query(ctx, args),
        "track_logical_model" => track_logical_model(ctx, args).await,
        "untrack_logical_model" => untrack_logical_model(ctx, args).await,
        "get_logical_model" => get_logical_model(ctx, args),
        "create_logical_model_select_permission" => create_lm_permission(ctx, args).await,
        "drop_logical_model_select_permission" => drop_lm_permission(ctx, args).await,
        _ => Err(super::not_implemented(command)),
    }
}

/// Is this object inconsistent in the current resolved state?
fn inconsistent<R: Runtime>(ctx: &Ctx<'_, R>, kind: &str, name: &str) -> bool {
    ctx.inconsistencies.iter().any(|o| o.id() == (kind, name))
}

/// The function a stored function or computed field definition refers to.
fn function_name(v: &Value) -> Option<QualifiedTable> {
    serde_json::from_value(v.clone()).ok()
}

fn function_id(source: &str, qf: &QualifiedTable) -> String {
    format!("function {} in source {source}", qf.v2_text())
}

fn computed_field_id(source: &str, table: &QualifiedTable, name: &str) -> String {
    format!(
        "computed_field {name} in table {} in source {source}",
        table.v2_text()
    )
}

fn logical_model_id(source: &str, name: &str) -> String {
    format!("logical_model {name} in source {source}")
}

fn native_query_id(source: &str, name: &str) -> String {
    format!("native_query {name} in source {source}")
}

/// Items of an array kept in a source's extra fields (`logical_models`, `native_queries`).
fn extra_items<'a>(source: &'a Source, key: &str) -> &'a [Value] {
    source
        .extra
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn source_arg(o: &Obj) -> ApiResult<String> {
    Ok(o.opt_string("source")?
        .unwrap_or_else(|| "default".to_owned()))
}

fn source_mut<'m, R: Runtime>(ctx: &'m mut Ctx<'_, R>, name: &str) -> &'m mut Source {
    ctx.metadata
        .source_mut(name)
        .expect("source existence is checked before editing")
}

fn extra_mut<'s>(source: &'s mut Source, key: &str) -> &'s mut Vec<Value> {
    let entry = source
        .extra
        .entry(key.to_owned())
        .or_insert_with(|| Value::Array(vec![]));
    if !entry.is_array() {
        *entry = Value::Array(vec![]);
    }
    match entry {
        Value::Array(items) => items,
        _ => unreachable!("replaced by an array above"),
    }
}

/// Drop an empty `logical_models` / `native_queries` list so it is not exported.
fn prune_extra(source: &mut Source, key: &str) {
    if source
        .extra
        .get(key)
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty)
    {
        source.extra.remove(key);
    }
}

// ---------------------------------------------------------------------------------------
// Functions

fn function_index(source: &Source, qf: &QualifiedTable) -> Option<usize> {
    source
        .functions
        .iter()
        .position(|f| f.get("function").and_then(function_name).as_ref() == Some(qf))
}

/// V2's `FunctionConfig`, keeping the keys that were given.
fn function_config(v: Option<&Value>, path: &str) -> ApiResult<Value> {
    let Some(v) = v else {
        return Ok(json!({}));
    };
    let o = object(v, path, "FunctionConfig")?;
    let mut out = Map::new();
    if let Some(s) = o.opt_string("session_argument")? {
        out.insert("session_argument".into(), Value::String(s));
    }
    if let Some(e) = o.opt_string("exposed_as")? {
        if e != "query" && e != "mutation" {
            return Err(parse_failed(
                &o.sub("exposed_as"),
                format!(
                    "parsing Hasura.Function.Cache.FunctionExposedAs failed, expected one of the tags [\"query\",\"mutation\"], but found tag \"{e}\""
                ),
            ));
        }
        out.insert("exposed_as".into(), Value::String(e));
    }
    if let Some(r) = o.get("custom_root_fields") {
        let r = object(r, &o.sub("custom_root_fields"), "FunctionCustomRootFields")?;
        let mut fields = Map::new();
        for key in ["function", "function_aggregate"] {
            if let Some(s) = r.opt_string(key)? {
                fields.insert(key.into(), Value::String(s));
            }
        }
        if !fields.is_empty() {
            out.insert("custom_root_fields".into(), Value::Object(fields));
        }
    }
    if let Some(n) = o.opt_string("custom_name")? {
        out.insert("custom_name".into(), Value::String(n));
    }
    // V2 always writes `custom_root_fields` of a non-empty configuration.
    if !out.is_empty() && !out.contains_key("custom_root_fields") {
        out.insert("custom_root_fields".into(), json!({}));
    }
    Ok(Value::Object(out))
}

async fn track_function<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let o = object(args, "$.args", "TrackFunctionV2")?;
    let source = source_arg(&o)?;
    let qf = qualified(o.req("function")?, &o.sub("function"))?;
    let config = function_config(o.get("configuration"), &o.sub("configuration"))?;
    let comment = o.opt_string("comment")?;
    if ctx.metadata.source(&source).is_none() {
        return Err(not_exists(format!(
            "\"{source}\" is not a known \"postgres\" source"
        )));
    }
    let id = function_id(&source, &qf);
    let retrack = inconsistent(ctx, "function", &id);
    let src = ctx.metadata.source(&source).expect("checked above");
    if function_index(src, &qf).is_some() && !retrack {
        return Err(ApiError::bad(
            Code::AlreadyTracked,
            format!("function already tracked: \"{}\"", qf.v2_text()),
        ));
    }
    if src.tables.iter().any(|t| t.table == qf) {
        return Err(ApiError::bad(
            Code::NotSupported,
            format!("table with name \"{}\" already exists", qf.v2_text()),
        ));
    }
    let mut entry = Map::new();
    entry.insert("function".into(), qualified_json(&qf));
    if config.as_object().is_some_and(|c| !c.is_empty()) {
        entry.insert("configuration".into(), config);
    }
    if let Some(c) = comment {
        entry.insert("comment".into(), Value::String(c));
    }
    let src = source_mut(ctx, &source);
    match function_index(src, &qf) {
        Some(i) => src.functions[i] = Value::Object(entry),
        None => src.functions.push(Value::Object(entry)),
    }
    ctx.check(Check::object("function", id)).await?;
    Ok(success())
}

/// A tracked, consistent function (V2's `askFunctionInfo`).
fn ask_function<R: Runtime>(
    ctx: &Ctx<'_, R>,
    source: &str,
    qf: &QualifiedTable,
) -> ApiResult<usize> {
    ctx.metadata
        .source(source)
        .and_then(|s| function_index(s, qf))
        .filter(|_| !inconsistent(ctx, "function", &function_id(source, qf)))
        .ok_or_else(|| {
            not_exists(format!(
                "function \"{}\" does not exist in source: {source}",
                qf.v2_text()
            ))
        })
}

async fn untrack_function<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let o = object(args, "$.args", "UnTrackFunction")?;
    let (source, qf) = match o.get("source") {
        Some(s) => (
            text(s, &o.sub("source"))?,
            qualified(o.req("function")?, &o.sub("function"))?,
        ),
        None => ("default".to_owned(), qualified(args, "$.args")?),
    };
    let idx = ask_function(ctx, &source, &qf)?;
    source_mut(ctx, &source).functions.remove(idx);
    ctx.check(Check::Newly).await?;
    Ok(success())
}

async fn set_function_customization<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let o = object(args, "$.args", "set function customization")?;
    let source = source_arg(&o)?;
    let qf = qualified(o.req("function")?, &o.sub("function"))?;
    let config = function_config(Some(o.req("configuration")?), &o.sub("configuration"))?;
    let idx = ask_function(ctx, &source, &qf)?;
    if let Value::Object(e) = &mut source_mut(ctx, &source).functions[idx] {
        if config.as_object().is_some_and(|c| !c.is_empty()) {
            e.insert("configuration".into(), config);
        } else {
            e.remove("configuration");
        }
    }
    ctx.check(Check::object("function", function_id(&source, &qf)))
        .await?;
    Ok(success())
}

fn function_permission_args(args: &Value) -> ApiResult<(String, QualifiedTable, String)> {
    let o = object(args, "$.args", "FunctionPermissionArgument")?;
    let qf = qualified(o.req("function")?, &o.sub("function"))?;
    let source = source_arg(&o)?;
    let role = o.string("role")?;
    Ok((source, qf, role))
}

fn function_permissions(entry: &Value) -> Vec<&str> {
    entry
        .get("permissions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| p.get("role").and_then(Value::as_str))
        .collect()
}

async fn create_function_permission<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let (source, qf, role) = function_permission_args(args)?;
    let idx = ask_function(ctx, &source, &qf)?;
    let src = ctx.metadata.source(&source).expect("the function exists");
    if function_permissions(&src.functions[idx]).contains(&role.as_str()) {
        return Err(already_exists(format!(
            "permission of role \"{role}\" already exists for function \"{}\" in source: \"{source}\"",
            qf.v2_text()
        )));
    }
    // The function's return table must give the role a permission (V2 looks the table up
    // in the schema cache). The function is consistent, so the catalog has its signature.
    let returned = ctx
        .rt
        .catalog(ctx.prepared.as_ref(), &source)
        .and_then(|c| c.functions.get(&qf).and_then(|f| f.first()).cloned())
        .map(|f| QualifiedTable::new(f.return_type.schema, f.return_type.name));
    if let Some(table) = returned {
        // V2 reads the resolved table's permissions, inherited roles included.
        let Some(roles) = ctx.rt.table_roles(ctx.prepared.as_ref(), &source, &table) else {
            return Err(not_exists(format!(
                "function's return table \"{}\" not found in the cache",
                table.v2_text()
            )));
        };
        let has_role = roles.contains(&role);
        if !has_role {
            return Err(ApiError::bad(
                Code::NotSupported,
                format!(
                    "function permission can only be added when the function's return table \"{}\" has select permission configured for role: \"{role}\"",
                    table.v2_text()
                ),
            ));
        }
    }
    if let Value::Object(e) = &mut source_mut(ctx, &source).functions[idx]
        && let Value::Array(items) = e
            .entry("permissions")
            .or_insert_with(|| Value::Array(vec![]))
    {
        items.insert(0, json!({ "role": role }));
    }
    ctx.check(Check::NoNew).await?;
    Ok(success())
}

async fn drop_function_permission<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let (source, qf, role) = function_permission_args(args)?;
    let idx = ctx.metadata.source(&source).and_then(|s| {
        function_index(s, &qf)
            .filter(|&i| function_permissions(&s.functions[i]).contains(&role.as_str()))
    });
    let Some(idx) = idx else {
        return Err(not_exists(format!(
            "permission of role \"{role}\" does not exist for function \"{}\" in source: \"{source}\"",
            qf.v2_text()
        )));
    };
    if let Value::Object(e) = &mut source_mut(ctx, &source).functions[idx]
        && let Some(Value::Array(items)) = e.get_mut("permissions")
    {
        items.retain(|p| p.get("role").and_then(Value::as_str) != Some(role.as_str()));
        if items.is_empty() {
            e.remove("permissions");
        }
    }
    ctx.check(Check::NoNew).await?;
    Ok(success())
}

// ---------------------------------------------------------------------------------------
// Computed fields

fn table_not_exists(table: &QualifiedTable, source: &str) -> ApiError {
    not_exists(format!(
        "table \"{}\" does not exist in source: {source}",
        table.v2_text()
    ))
    .with_path("$.args.table")
}

async fn add_computed_field<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let o = object(args, "$.args", "AddComputedField")?;
    let source = source_arg(&o)?;
    let table = qualified(o.req("table")?, &o.sub("table"))?;
    let name = o.string("name")?;
    let d = record(
        o.req("definition")?,
        &o.sub("definition"),
        "Hasura.Backends.Postgres.Types.ComputedField.ComputedFieldDefinition(ComputedFieldDefinition)",
    )?;
    let qf = qualified(d.req("function")?, &d.sub("function"))?;
    let mut definition = Map::new();
    definition.insert("function".into(), qualified_json(&qf));
    for key in ["table_argument", "session_argument"] {
        if let Some(s) = d.opt_string(key)? {
            definition.insert(key.into(), Value::String(s));
        }
    }
    let comment = o.opt_string("comment")?;
    let exists = ctx
        .metadata
        .source(&source)
        .is_some_and(|s| s.tables.iter().any(|t| t.table == table));
    if !exists {
        return Err(table_not_exists(&table, &source));
    }
    let mut cf = Map::new();
    cf.insert("name".into(), Value::String(name.clone()));
    cf.insert("definition".into(), Value::Object(definition));
    if let Some(c) = comment {
        cf.insert("comment".into(), Value::String(c));
    }
    let entry = source_mut(ctx, &source)
        .tables
        .iter_mut()
        .find(|t| t.table == table)
        .expect("checked above");
    match entry
        .computed_fields
        .iter()
        .position(|c| c.get("name").and_then(Value::as_str) == Some(name.as_str()))
    {
        Some(i) => entry.computed_fields[i] = Value::Object(cf),
        None => entry.computed_fields.push(Value::Object(cf)),
    }
    let id = computed_field_id(&source, &table, &name);
    ctx.check(Check::object("computed_field", id)).await?;
    Ok(success())
}

async fn drop_computed_field<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let o = object(args, "$.args", "DropComputedField")?;
    let source = source_arg(&o)?;
    let table = qualified(o.req("table")?, &o.sub("table"))?;
    let name = o.string("name")?;
    let cascade = o.bool_or("cascade", false)?;
    let Some(entry) = ctx
        .metadata
        .source(&source)
        .and_then(|s| s.tables.iter().find(|t| t.table == table))
    else {
        return Err(table_not_exists(&table, &source));
    };
    if !entry
        .computed_fields
        .iter()
        .any(|c| c.get("name").and_then(Value::as_str) == Some(name.as_str()))
    {
        return Err(
            not_exists(format!("computed field \"{name}\" does not exist"))
                .with_path("$.args.name"),
        );
    }
    // Permissions whose columns or row filters use the computed field, on this table or
    // through relationships and `_exists` from other tables.
    let src = ctx.metadata.source(&source).expect("checked above");
    let catalog = ctx.rt.catalog(ctx.prepared.as_ref(), &source);
    let rels = Rels::from_catalog(&src.tables, catalog.as_deref());
    let mut dependents: Vec<(QualifiedTable, String, &str)> = vec![];
    for t in &src.tables {
        let uses = |e: &Value| bool_exp_mentions(e, &t.table, &rels, &table, &name);
        let mut add = |role: &str, kind| dependents.push((t.table.clone(), role.to_owned(), kind));
        for p in &t.select_permissions {
            let own = t.table == table && p.permission.computed_fields.contains(&name);
            if own || uses(&p.permission.filter) {
                add(&p.role, "select");
            }
        }
        for p in &t.insert_permissions {
            if uses(&p.permission.check) {
                add(&p.role, "insert");
            }
        }
        for p in &t.update_permissions {
            if uses(&p.permission.filter) || p.permission.check.as_ref().is_some_and(uses) {
                add(&p.role, "update");
            }
        }
        for p in &t.delete_permissions {
            if uses(&p.permission.filter) {
                add(&p.role, "delete");
            }
        }
    }
    if !dependents.is_empty() && !cascade {
        let mut list: Vec<String> = dependents
            .iter()
            .map(|(t, role, kind)| {
                format!(
                    "permission {}.{role}.{kind} in source \"{source}\"",
                    t.v2_text()
                )
            })
            .collect();
        list.sort();
        return Err(ApiError::bad(
            Code::DependencyError,
            format!(
                "cannot drop due to the following dependent objects: {}",
                list.join(", ")
            ),
        ));
    }
    for t in &mut source_mut(ctx, &source).tables {
        let dropped = |role: &str, kind: &str| {
            dependents
                .iter()
                .any(|(dt, r, k)| *dt == t.table && r == role && *k == kind)
        };
        t.select_permissions.retain(|p| !dropped(&p.role, "select"));
        t.insert_permissions.retain(|p| !dropped(&p.role, "insert"));
        t.update_permissions.retain(|p| !dropped(&p.role, "update"));
        t.delete_permissions.retain(|p| !dropped(&p.role, "delete"));
    }
    let entry = source_mut(ctx, &source)
        .tables
        .iter_mut()
        .find(|t| t.table == table)
        .expect("checked above");
    entry
        .computed_fields
        .retain(|c| c.get("name").and_then(Value::as_str) != Some(name.as_str()));
    ctx.check(Check::Newly).await?;
    Ok(success())
}

// ---------------------------------------------------------------------------------------
// Logical models

/// V2's scalar types of logical models and native query arguments (`decodePGScalarType`),
/// normalised to their canonical names.
fn scalar_type(t: &str) -> Option<String> {
    const TRANSLATIONS: &[(&str, &str)] = &[
        ("serial", "serial"),
        ("bigserial", "bigserial"),
        ("smallint", "smallint"),
        ("int2", "smallint"),
        ("int", "integer"),
        ("integer", "integer"),
        ("int4", "integer"),
        ("bigint", "bigint"),
        ("int8", "bigint"),
        ("real", "real"),
        ("float4", "real"),
        ("double precision", "float8"),
        ("float8", "float8"),
        ("numeric", "numeric"),
        ("decimal", "numeric"),
        ("money", "money"),
        ("boolean", "boolean"),
        ("bool", "boolean"),
        ("bpchar", "bpchar"),
        ("char", "bpchar"),
        ("character", "bpchar"),
        ("varchar", "varchar"),
        ("character varying", "varchar"),
        ("text", "text"),
        ("citext", "citext"),
        ("date", "date"),
        ("timestamp", "timestamp"),
        ("timestamp without time zone", "timestamp"),
        ("timestamptz", "timestamptz"),
        ("timestamp with time zone", "timestamptz"),
        ("timetz", "timetz"),
        ("time with time zone", "timetz"),
        ("json", "json"),
        ("jsonb", "jsonb"),
        ("geometry", "geometry"),
        ("geography", "geography"),
        ("raster", "raster"),
        ("uuid", "uuid"),
        ("ltree", "ltree"),
        ("lquery", "lquery"),
        ("ltxtquery", "ltxtquery"),
    ];
    const UNKNOWNS: &[&str] = &[
        "bit varying",
        "bit",
        "box",
        "bytea",
        "cidr",
        "circle",
        "inet",
        "interval",
        "line",
        "lseg",
        "macaddr",
        "macaddr8",
        "path",
        "pg_lsn",
        "point",
        "polygon",
        "serial2",
        "serial4",
        "smallserial",
        "time without time zone",
        "time",
        "tsquery",
        "tsvector",
        "txid_snapshot",
        "varbit",
        "xml",
    ];
    let lower = t.to_lowercase();
    let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    // `varchar(10)` / `numeric(10, 2)` mean the type without its precision.
    let strip = |prefixes: &[&str], two: bool| -> Option<String> {
        prefixes.iter().find_map(|p| {
            let rest = lower.strip_prefix(p)?.trim_start_matches(' ');
            let inner = rest.strip_prefix('(')?.strip_suffix(')')?;
            let ok = if two {
                inner.split_once(',').is_some_and(|(a, b)| {
                    digits(a.trim_end_matches(' ')) && digits(b.trim_start_matches(' '))
                })
            } else {
                digits(inner)
            };
            ok.then(|| (*p).to_owned())
        })
    };
    let massaged = strip(
        &[
            "bit varying",
            "character varying",
            "bit",
            "varbit",
            "char",
            "character",
            "varchar",
        ],
        false,
    )
    .or_else(|| strip(&["numeric", "decimal"], true))
    .unwrap_or(lower);
    if let Some((_, canonical)) = TRANSLATIONS.iter().find(|(k, _)| *k == massaged) {
        return Some((*canonical).to_owned());
    }
    UNKNOWNS.contains(&massaged.as_str()).then_some(massaged)
}

/// `nullable` of a type object (defaults to `false`).
fn nullable(o: &Map<String, Value>) -> Option<bool> {
    match o.get("nullable") {
        None => Some(false),
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => None,
    }
}

/// V2's `LogicalModelType` (the new field format), normalised.
fn field_type(v: &Value) -> Option<Value> {
    let o = v.as_object()?;
    if let Some(s) = o.get("scalar").and_then(Value::as_str)
        && let (Some(t), Some(n)) = (scalar_type(s), nullable(o))
    {
        return Some(json!({"nullable": n, "scalar": t}));
    }
    if let Some(inner) = o.get("array")
        && let (Some(t), Some(n)) = (field_type(inner), nullable(o))
    {
        return Some(json!({"array": t, "nullable": n}));
    }
    if let Some(r) = o.get("logical_model").and_then(Value::as_str)
        && let Some(n) = nullable(o)
    {
        return Some(json!({"logical_model": r, "nullable": n}));
    }
    None
}

/// A logical model field, in V2's (new) form.
fn logical_model_field(v: &Value, path: &str) -> ApiResult<Value> {
    let o = object(v, path, "PostgresLogicalModelField")?;
    // Both of V2's field formats failed: it reports the last error of each.
    let fail = |inner: String, simple: String, at: String| {
        parse_failed(
            &at,
            format!("\n  Previous branch failure: {inner}\n{simple}"),
        )
    };
    let name = match o.get("name") {
        Some(Value::String(s)) => s.clone(),
        _ => {
            let m = "key \"name\" not found".to_owned();
            return Err(fail(format!("Error in $: {m}"), m, path.to_owned()));
        }
    };
    let Some(ty) = o.map.get("type") else {
        let m = "key \"type\" not found".to_owned();
        return Err(fail(format!("Error in $: {m}"), m, path.to_owned()));
    };
    let typ = match field_type(ty) {
        Some(t) => t,
        None => {
            // The simple form: `type` is a scalar name and `nullable` sits on the field.
            let simple = match ty {
                Value::String(s) => scalar_type(s)
                    .map(|t| json!({"nullable": nullable(o.map).unwrap_or(false), "scalar": t}))
                    .ok_or_else(|| format!("Did not recognize scalar type '{s}'")),
                other => Err(format!(
                    "parsing Text failed, expected String, but encountered {}",
                    json_kind(other)
                )),
            };
            match simple {
                Ok(t) => t,
                Err(simple) => {
                    let reference = if ty.is_object() {
                        "key \"logical_model\" not found".to_owned()
                    } else {
                        format!(
                            "parsing LogicalModelTypeReference failed, expected Object, but encountered {}",
                            json_kind(ty)
                        )
                    };
                    return Err(fail(
                        format!("Error in $.type: Error in $: {reference}"),
                        simple,
                        format!("{path}.type"),
                    ));
                }
            }
        }
    };
    let mut out = Map::new();
    out.insert("name".into(), Value::String(name));
    out.insert("type".into(), typ);
    if let Some(d) = o.opt_string("description")? {
        out.insert("description".into(), Value::String(d));
    }
    Ok(Value::Object(out))
}

fn find_named(items: &[Value], key: &str, name: &str) -> Option<usize> {
    items
        .iter()
        .position(|i| i.get(key).and_then(Value::as_str) == Some(name))
}

/// `Source "x" not found.` / `Logical model "y" not found in source "x".`
fn assert_logical_model<R: Runtime>(
    ctx: &Ctx<'_, R>,
    source: &str,
    name: &str,
) -> ApiResult<usize> {
    let Some(src) = ctx.metadata.source(source) else {
        return Err(ApiError::bad(
            Code::NotFound,
            format!("Source \"{source}\" not found."),
        ));
    };
    find_named(extra_items(src, "logical_models"), "name", name).ok_or_else(|| {
        ApiError::bad(
            Code::NotFound,
            format!("Logical model \"{name}\" not found in source \"{source}\"."),
        )
    })
}

async fn track_logical_model<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let o = object(args, "$.args", "PostgresTrackLogicalModel")?;
    let source = o.string("source")?;
    let name = o.name("name")?;
    let description = o.opt_string("description")?;
    let fields_v = o.array("fields")?;
    let mut fields = vec![];
    for (i, f) in fields_v.iter().enumerate() {
        fields.push(logical_model_field(
            f,
            &format!("{}[{i}]", o.sub("fields")),
        )?);
    }
    let Some(src) = ctx.metadata.source(&source) else {
        return Err(ApiError::bad(
            Code::NotFound,
            format!("Source {source} not found."),
        ));
    };
    if find_named(extra_items(src, "logical_models"), "name", &name).is_some() {
        return Err(ApiError::bad(
            Code::AlreadyTracked,
            format!("Logical model '{name}' is already tracked."),
        ));
    }
    let mut model = Map::new();
    model.insert("name".into(), Value::String(name.clone()));
    model.insert("fields".into(), Value::Array(fields));
    if let Some(d) = description {
        model.insert("description".into(), Value::String(d));
    }
    extra_mut(source_mut(ctx, &source), "logical_models").push(Value::Object(model));
    let id = logical_model_id(&source, &name);
    ctx.check(Check::object("logical_model", id)).await?;
    Ok(success())
}

async fn untrack_logical_model<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let o = object(args, "$.args", "UntrackLogicalModel")?;
    let source = o.string("source")?;
    let name = o.string("name")?;
    let idx = assert_logical_model(ctx, &source, &name)?;
    let src = source_mut(ctx, &source);
    extra_mut(src, "logical_models").remove(idx);
    prune_extra(src, "logical_models");
    ctx.check(Check::NoNew).await?;
    Ok(success())
}

/// `get_logical_model` / `get_native_query`: the stored objects in V2's field order.
fn get_items<R: Runtime>(
    ctx: &Ctx<'_, R>,
    args: &Value,
    ty: &str,
    key: &str,
    order: &[&str],
) -> ApiResult<Value> {
    let o = object(args, "$.args", ty)?;
    let source = o.string("source")?;
    let Some(src) = ctx.metadata.source(&source) else {
        return Err(ApiError::bad(
            Code::NotFound,
            format!("Source '{source}' of kind postgres not found."),
        ));
    };
    let items = extra_items(src, key)
        .iter()
        .map(|item| reorder(item, order))
        .collect();
    Ok(Value::Array(items))
}

fn reorder(item: &Value, order: &[&str]) -> Value {
    let Some(obj) = item.as_object() else {
        return item.clone();
    };
    let mut out = Map::new();
    for key in order {
        if let Some(v) = obj.get(*key) {
            out.insert((*key).to_owned(), v.clone());
        }
    }
    for (k, v) in obj {
        if !out.contains_key(k) {
            out.insert(k.clone(), v.clone());
        }
    }
    Value::Object(out)
}

fn get_logical_model<R: Runtime>(ctx: &Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    get_items(
        ctx,
        args,
        "GetLogicalModel",
        "logical_models",
        &["name", "fields", "description", "select_permissions"],
    )
}

/// The logical model a permission request is about. Permissions of a native query's inline
/// logical model (`"type": "native_query"`) are not supported.
fn permission_target(o: &Obj) -> ApiResult<(String, String)> {
    let source = source_arg(o)?;
    match o.get("type") {
        None => {}
        Some(Value::String(s)) if s == "logical_model" => {}
        Some(Value::String(s)) if s == "native_query" => {
            return Err(super::not_implemented(
                "logical model permissions of native queries",
            ));
        }
        Some(_) => return Err(parse_failed(&o.path, "Expected string")),
    }
    Ok((source, o.string("name")?))
}

async fn create_lm_permission<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let o = object(args, "$.args", "CreateLogicalModelPermission")?;
    let (source, name) = permission_target(&o)?;
    let role = o.string("role")?;
    let p = object(o.req("permission")?, &o.sub("permission"), "SelPerm")?;
    p.req("columns")?;
    let permission = Value::Object(
        p.map
            .iter()
            .filter(|(_, v)| !v.is_null())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    );
    let comment = o.opt_string("comment")?;
    let idx = assert_logical_model(ctx, &source, &name)?;
    let mut entry = Map::new();
    entry.insert("role".into(), Value::String(role.clone()));
    entry.insert("permission".into(), permission);
    if let Some(c) = comment {
        entry.insert("comment".into(), Value::String(c));
    }
    if let Value::Object(m) = &mut extra_mut(source_mut(ctx, &source), "logical_models")[idx]
        && let Value::Array(items) = m
            .entry("select_permissions")
            .or_insert_with(|| Value::Array(vec![]))
    {
        match find_named(items, "role", &role) {
            Some(i) => items[i] = Value::Object(entry),
            None => items.push(Value::Object(entry)),
        }
    }
    let id = logical_model_id(&source, &name);
    ctx.check(Check::object("logical_model", id)).await?;
    Ok(success())
}

async fn drop_lm_permission<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let o = object(args, "$.args", "DropLogicalModelPermission")?;
    let (source, name) = permission_target(&o)?;
    let role = o.string("role")?;
    let idx = assert_logical_model(ctx, &source, &name)?;
    if let Value::Object(m) = &mut extra_mut(source_mut(ctx, &source), "logical_models")[idx]
        && let Some(Value::Array(items)) = m.get_mut("select_permissions")
    {
        items.retain(|p| p.get("role").and_then(Value::as_str) != Some(role.as_str()));
        if items.is_empty() {
            m.remove("select_permissions");
        }
    }
    let id = logical_model_id(&source, &name);
    ctx.check(Check::object("logical_model", id)).await?;
    Ok(success())
}

// ---------------------------------------------------------------------------------------
// Native queries

/// A native query argument (V2's `NullableScalarType`).
fn argument(v: &Value, path: &str) -> ApiResult<Value> {
    let o = object(v, path, "PostgresNullableScalarType")?;
    let t = o.string("type")?;
    let Some(ty) = scalar_type(&t) else {
        return Err(parse_failed(
            &o.sub("type"),
            format!("Did not recognize scalar type '{t}'"),
        ));
    };
    let mut out = Map::new();
    out.insert("type".into(), Value::String(ty));
    out.insert(
        "nullable".into(),
        Value::Bool(o.bool_or("nullable", false)?),
    );
    if let Some(d) = o.opt_string("description")? {
        out.insert("description".into(), Value::String(d));
    }
    Ok(Value::Object(out))
}

/// Native query relationships (`RelDef RelManualNativeQueryConfig`).
fn relationships(v: &Value, path: &str) -> ApiResult<Vec<Value>> {
    let items = crate::ops::parse::array(v, path)?;
    let mut out = vec![];
    for (i, r) in items.iter().enumerate() {
        let p = format!("{path}[{i}]");
        let o = object(r, &p, "RelDef")?;
        let u = object(
            o.req("using")?,
            &o.sub("using"),
            "RelManualNativeQueryConfig",
        )?;
        let mut rel = Map::new();
        rel.insert("name".into(), Value::String(o.string("name")?));
        rel.insert(
            "using".into(),
            json!({
                "remote_native_query": u.string("remote_native_query")?,
                "column_mapping": u.req("column_mapping")?.clone(),
                "insertion_order": u.get("insertion_order").cloned().unwrap_or(Value::Null),
            }),
        );
        if let Some(c) = o.opt_string("comment")? {
            rel.insert("comment".into(), Value::String(c));
        }
        out.push(Value::Object(rel));
    }
    Ok(out)
}

/// V2's `parseInterpolatedQuery`: every `{{` needs a matching `}}`. Returns the argument
/// names used.
fn interpolate(code: &str) -> Result<Vec<&str>, String> {
    let mut vars = vec![];
    let mut rest = code;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let end = match after.find('}') {
            Some(e) if after[e..].starts_with("}}") => e,
            _ => return Err("Found '{{' without a matching closing '}}'".into()),
        };
        vars.push(&after[..end]);
        rest = &after[end + 2..];
    }
    Ok(vars)
}

/// What a native query returns: a logical model's name, or an inline logical model
/// (`PostgresInlineLogicalModelMetadata`). V2 decodes it with aeson at `path`, so errors
/// inside it carry aeson's relative path in the message.
fn native_query_returns(v: &Value, path: &str) -> ApiResult<Value> {
    let fail = |at: &str, msg: String| {
        let nl = if msg.starts_with('\n') { "\n" } else { "" };
        parse_failed(path, format!("{nl}Error in {at}: {msg}"))
    };
    let o = match v {
        Value::String(_) => return Ok(v.clone()),
        Value::Object(o) => o,
        other => {
            return Err(fail(
                "$",
                format!(
                    "parsing PostgresInlineLogicalModelMetadata failed, expected Object, but encountered {}",
                    json_kind(other)
                ),
            ));
        }
    };
    let fields = match o.get("fields") {
        None => return Err(fail("$", "key \"fields\" not found".into())),
        Some(Value::Array(items)) => items,
        Some(other) => {
            return Err(fail(
                "$.fields",
                format!(
                    "parsing Vector failed, expected Array, but encountered {}",
                    json_kind(other)
                ),
            ));
        }
    };
    let fields = fields
        .iter()
        .enumerate()
        .map(|(i, f)| {
            logical_model_field(f, &format!("$.fields[{i}]"))
                .map_err(|e| fail(&e.error.path, e.error.message))
        })
        .collect::<ApiResult<Vec<_>>>()?;
    let mut out = Map::new();
    out.insert("fields".into(), Value::Array(fields));
    for key in ["select_permissions"] {
        if let Some(v) = o.get(key).filter(|v| !v.is_null()) {
            out.insert(key.into(), v.clone());
        }
    }
    Ok(Value::Object(out))
}

async fn track_native_query<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let o = object(args, "$.args", "PostgresTrackNativeQuery")?;
    let source = o.string("source")?;
    let name = o.name("root_field_name")?;
    let code = o.string("code")?;
    let mut arguments = Map::new();
    if let Some(a) = o.get("arguments") {
        let a = object(a, &o.sub("arguments"), "HashMap")?;
        let mut keys: Vec<&String> = a.map.keys().collect();
        keys.sort();
        for k in keys {
            arguments.insert(k.clone(), argument(&a.map[k], &a.sub(k))?);
        }
    }
    let array_rels = match o.get("array_relationships") {
        Some(v) => relationships(v, &o.sub("array_relationships"))?,
        None => vec![],
    };
    let object_rels = match o.get("object_relationships") {
        Some(v) => relationships(v, &o.sub("object_relationships"))?,
        None => vec![],
    };
    let description = o.opt_string("description")?;
    let returns = native_query_returns(o.req("returns")?, &o.sub("returns"))?;
    let Some(src) = ctx.metadata.source(&source) else {
        return Err(ApiError::bad(
            Code::NotFound,
            format!("Source '{source}' of kind postgres not found."),
        ));
    };
    if let Err(e) = interpolate(&code) {
        return Err(parse_failed("$.args", e));
    }
    if find_named(extra_items(src, "native_queries"), "root_field_name", &name).is_some() {
        return Err(ApiError::bad(
            Code::AlreadyTracked,
            format!("Native query '{name}' is already tracked."),
        ));
    }
    // V2's `NativeQueryMetadata` field order (`get_native_query` shows it).
    let mut nq = Map::new();
    nq.insert("root_field_name".into(), Value::String(name.clone()));
    nq.insert("code".into(), Value::String(code));
    nq.insert("returns".into(), returns);
    nq.insert("arguments".into(), Value::Object(arguments));
    if !array_rels.is_empty() {
        nq.insert("array_relationships".into(), Value::Array(array_rels));
    }
    if !object_rels.is_empty() {
        nq.insert("object_relationships".into(), Value::Array(object_rels));
    }
    if let Some(d) = description {
        nq.insert("description".into(), Value::String(d));
    }
    extra_mut(source_mut(ctx, &source), "native_queries").push(Value::Object(nq));
    let id = native_query_id(&source, &name);
    ctx.check(Check::object("native_query", id)).await?;
    Ok(success())
}

async fn untrack_native_query<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let o = object(args, "$.args", "UntrackNativeQuery")?;
    let source = o.string("source")?;
    let name = o.string("root_field_name")?;
    let Some(src) = ctx.metadata.source(&source) else {
        return Err(ApiError::bad(
            Code::NotFound,
            format!("Source \"{source}\" not found."),
        ));
    };
    let Some(idx) = find_named(extra_items(src, "native_queries"), "root_field_name", &name) else {
        return Err(ApiError::bad(
            Code::NotFound,
            format!("Native query \"{name}\" not found in source \"{source}\"."),
        ));
    };
    let src = source_mut(ctx, &source);
    extra_mut(src, "native_queries").remove(idx);
    prune_extra(src, "native_queries");
    ctx.check(Check::NoNew).await?;
    Ok(success())
}

fn get_native_query<R: Runtime>(ctx: &Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    get_items(
        ctx,
        args,
        "GetNativeQuery",
        "native_queries",
        &[
            "root_field_name",
            "code",
            "returns",
            "arguments",
            "array_relationships",
            "object_relationships",
            "description",
        ],
    )
    .map(|mut v| {
        // Metadata loaded through replace_metadata keeps its own key order; V2 re-encodes
        // the arguments and relationships too.
        for nq in v.as_array_mut().into_iter().flatten() {
            if let Some(args) = nq.get_mut("arguments").and_then(Value::as_object_mut) {
                for a in args.values_mut() {
                    *a = reorder(a, &["type", "nullable", "description"]);
                }
            }
            for key in ["array_relationships", "object_relationships"] {
                for r in nq
                    .get_mut(key)
                    .and_then(Value::as_array_mut)
                    .into_iter()
                    .flatten()
                {
                    *r = reorder(r, &["name", "using", "comment"]);
                    if let Some(u) = r.get_mut("using") {
                        *u = reorder(
                            u,
                            &["remote_native_query", "column_mapping", "insertion_order"],
                        );
                    }
                }
            }
        }
        v
    })
}
