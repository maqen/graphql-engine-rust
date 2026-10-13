//! Action and custom type commands (spec 06 §5, V2's `RQL/DDL/Action.hs` and
//! `RQL/DDL/CustomTypes.hs`), and `test_webhook_transform`.
//!
//! Actions and custom types are kept in the metadata as JSON. The commands parse their
//! arguments with the typed views of `hge_metadata::actions` and store them the way V2
//! serialises its parsed metadata (defaults filled in, unknown keys dropped), so
//! `export_metadata` shows what V2 would. Whether an action resolves is left to the schema
//! build (`ctx.check`).

use hge_core::Code;
use hge_metadata::actions::{
    ActionDefinition, ActionKind, ActionType, CustomTypes, DEFAULT_ACTION_TIMEOUT_SECS, HeaderConf,
    RelType, strip_nulls,
};
use hge_transforms::{ParseError, RequestTransform, ResponseTransform};
use serde_json::{Map, Value, json};

use crate::api::json_kind;
use crate::ctx::{Check, Ctx};
use crate::response::{ApiError, ApiResult, already_exists, not_exists, parse_failed, success};
use crate::runtime::Runtime;

/// `test_webhook_transform` (spec 06 §12): apply a request transform to a sample request
/// and return it. Every failure is a 400, as in V2 (`err400`), whatever its code.
pub fn test_webhook_transform(args: &Value) -> ApiResult<Value> {
    hge_transforms::test_webhook_transform(args).map_err(|error| ApiError { status: 400, error })
}

/// On `/v1/query` V2 decodes these commands' `args` without adding `args` to the error
/// path (`$.definition.headers[0]`, not `$.args.definition.headers[0]`).
pub fn relative_to_args(command: &str, mut e: ApiError) -> ApiError {
    const COMMANDS: &[&str] = &[
        "create_action",
        "update_action",
        "drop_action",
        "create_action_permission",
        "drop_action_permission",
        "set_custom_types",
    ];
    if COMMANDS.contains(&command)
        && e.error.code == Code::ParseFailed
        && let Some(rest) = e.error.path.strip_prefix(ARGS)
        && !rest.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
    {
        e.error.path = format!("${rest}");
    }
    e
}

/// `create_action {name, definition, comment?}`.
pub async fn create_action<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let (name, definition, comment) = action_args(args, "CreateAction")?;
    if in_schema_cache(ctx, &name) {
        return Err(already_exists(format!(
            "action with name \"{name}\" already exists"
        )));
    }
    let action = action_json(&name, definition, comment, None);
    // V2 inserts into a map: an (inconsistent) action of that name is replaced in place.
    match action_index(&ctx.metadata.actions, &name) {
        Some(i) => ctx.metadata.actions[i] = action,
        None => ctx.metadata.actions.push(action),
    }
    ctx.check(Check::object("action", format!("action {name}")))
        .await?;
    Ok(success())
}

/// `update_action {name, definition, comment?}`: replaces the definition and the comment
/// (an omitted comment clears it) and keeps the permissions.
pub async fn update_action<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let (name, definition, comment) = action_args(args, "UpdateAction")?;
    if !in_schema_cache(ctx, &name) {
        return Err(does_not_exist(&name));
    }
    if let Some(i) = action_index(&ctx.metadata.actions, &name) {
        let permissions = ctx.metadata.actions[i].get("permissions").cloned();
        ctx.metadata.actions[i] = action_json(&name, definition, comment, permissions);
    }
    ctx.check(Check::object("action", format!("action {name}")))
        .await?;
    Ok(success())
}

/// `drop_action {name, clear_data?}`. Unless `clear_data` is `false`, the action's async
/// action log is deleted after the rebuild (right away, like V2: a later failure of the
/// request does not bring it back).
pub async fn drop_action<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let (name, clear_data) = drop_args(args)?;
    if !in_schema_cache(ctx, &name) {
        return Err(does_not_exist(&name));
    }
    ctx.metadata
        .actions
        .retain(|a| a.get("name").and_then(Value::as_str) != Some(name.as_str()));
    // V2's `withNewInconsistentObjsCheck`, a 500.
    if let Err(mut e) = ctx.check(Check::NoNew).await {
        if e.error.code == Code::Unexpected {
            e.status = 500;
            e.error.message = "cannot continue due to newly found inconsistent metadata".into();
        }
        return Err(e);
    }
    // The request's `resource_version` was checked before any command ran
    // (`Ctx::expect_version` in `api::handle`), so a stale request never gets here. V2
    // only compares it when it writes the metadata, after this delete; checking first is
    // safer and otherwise the same.
    if clear_data {
        ctx.rt.store().delete_action_logs(&name).await?;
    }
    Ok(success())
}

/// `{name, clear_data?}` of `drop_action`; `clear_data` defaults to `true`.
fn drop_args(args: &Value) -> ApiResult<(String, bool)> {
    const RECORD: &str = "Hasura.RQL.DDL.Action.DropAction(DropAction)";
    let obj = record(args, RECORD)?;
    let name = graphql_name(obj, "name", RECORD)?;
    let clear_data = opt_bool(obj, "clear_data", ARGS)?.unwrap_or(true);
    Ok((name, clear_data))
}

/// `create_action_permission {action, role, definition?, comment?}`. `definition` is
/// ignored. The permission is prepended; an unknown action is silently left alone (V2's
/// `ix` modifier).
pub async fn create_action_permission<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    const RECORD: &str = "Hasura.RQL.DDL.Action.CreateActionPermission(CreateActionPermission)";
    let obj = record(args, RECORD)?;
    let action = graphql_name(obj, "action", RECORD)?;
    let role = role_name(obj, "role", RECORD)?;
    let comment = optional_text(obj, "comment")?;
    if has_permission(&ctx.metadata.actions, &action, &role) {
        return Err(already_exists(format!(
            "permission for role \"{role}\" is already defined on \"{action}\""
        )));
    }
    if let Some(i) = action_index(&ctx.metadata.actions, &action)
        && let Value::Object(a) = &mut ctx.metadata.actions[i]
    {
        let mut permission = Map::new();
        permission.insert("role".into(), Value::String(role));
        if let Some(c) = comment {
            permission.insert("comment".into(), Value::String(c));
        }
        let mut permissions = match a.remove("permissions") {
            Some(Value::Array(p)) => p,
            _ => vec![],
        };
        permissions.insert(0, Value::Object(permission));
        a.insert("permissions".into(), Value::Array(permissions));
    }
    ctx.check(Check::NoNew).await?;
    Ok(success())
}

/// `drop_action_permission {action, role}`.
pub async fn drop_action_permission<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    const RECORD: &str = "Hasura.RQL.DDL.Action.DropActionPermission(DropActionPermission)";
    let obj = record(args, RECORD)?;
    let action = graphql_name(obj, "action", RECORD)?;
    let role = role_name(obj, "role", RECORD)?;
    if !has_permission(&ctx.metadata.actions, &action, &role) {
        return Err(not_exists(format!(
            "permission for role: \"{role}\" is not defined on \"{action}\""
        )));
    }
    if let Some(i) = action_index(&ctx.metadata.actions, &action)
        && let Value::Object(a) = &mut ctx.metadata.actions[i]
    {
        let remaining: Vec<Value> = match a.remove("permissions") {
            Some(Value::Array(p)) => p
                .into_iter()
                .filter(|p| role_of(p) != Some(role.as_str()))
                .collect(),
            _ => vec![],
        };
        if !remaining.is_empty() {
            a.insert("permissions".into(), Value::Array(remaining));
        }
    }
    ctx.check(Check::NoNew).await?;
    Ok(success())
}

/// `set_custom_types`: replace all custom types (`{}` clears them). Custom types that do
/// not validate fail with `invalid-configuration`; valid ones that break an action fail
/// with `cannot continue due to new inconsistent metadata`.
pub async fn set_custom_types<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    ctx.metadata.custom_types = parse_custom_types(args)?;
    ctx.check(Check::object("custom_types", "custom_types custom_types"))
        .await?;
    Ok(success())
}

fn does_not_exist(name: &str) -> ApiError {
    not_exists(format!("action with name \"{name}\" does not exist"))
}

/// Is the action in the schema cache, i.e. in the metadata and consistent?
fn in_schema_cache<R: Runtime>(ctx: &Ctx<'_, R>, name: &str) -> bool {
    let id = format!("action {name}");
    action_index(&ctx.metadata.actions, name).is_some()
        && !ctx
            .inconsistencies
            .iter()
            .any(|o| o.kind == "action" && o.name == id)
}

fn action_index(actions: &[Value], name: &str) -> Option<usize> {
    actions
        .iter()
        .position(|a| a.get("name").and_then(Value::as_str) == Some(name))
}

fn role_of(permission: &Value) -> Option<&str> {
    permission.get("role").and_then(Value::as_str)
}

fn has_permission(actions: &[Value], action: &str, role: &str) -> bool {
    action_index(actions, action)
        .and_then(|i| actions[i].get("permissions"))
        .and_then(Value::as_array)
        .is_some_and(|p| p.iter().any(|p| role_of(p) == Some(role)))
}

/// The stored form of an action (V2's `actionMetadataToOrdJSON` content).
fn action_json(
    name: &str,
    definition: Value,
    comment: Option<String>,
    permissions: Option<Value>,
) -> Value {
    let mut a = Map::new();
    a.insert("name".into(), Value::String(name.to_owned()));
    a.insert("definition".into(), definition);
    if let Some(c) = comment {
        a.insert("comment".into(), Value::String(c));
    }
    if let Some(p) = permissions.filter(|p| p.as_array().is_some_and(|p| !p.is_empty())) {
        a.insert("permissions".into(), p);
    }
    Value::Object(a)
}

/// `{name, definition, comment?}` of `create_action` / `update_action`: the name, the
/// stored definition and the comment. The transforms' templates are parse-checked
/// (V2's `validateTransforms`).
fn action_args(args: &Value, record_name: &str) -> ApiResult<(String, Value, Option<String>)> {
    let record_name = format!("Hasura.RQL.DDL.Action.{record_name}({record_name})");
    let obj = record(args, &record_name)?;
    let name = graphql_name(obj, "name", &record_name)?;
    let raw = required(obj, "definition", ARGS, Rec::Generic(&record_name))?;
    let (definition, parsed) = parse_definition(raw)?;
    let comment = optional_text(obj, "comment")?;
    hge_transforms::validate_transforms(
        parsed.request_transform.as_ref(),
        parsed.response_transform.as_ref(),
    )?;
    Ok((name, definition, comment))
}

const ARGS: &str = "$.args";
const ARGUMENT: Rec<'static> =
    Rec::Generic("Hasura.RQL.Types.Action.ArgumentDefinition(ArgumentDefinition)");
const MUTATION_KIND: &str = "Hasura.RQL.Types.Action.ActionMutationKind";
const INPUT_OBJECT: Rec<'static> = Rec::Th("InputObjectTypeDefinition");
const INPUT_FIELD: Rec<'static> = Rec::Th("InputObjectFieldDefinition");
const OBJECT_TYPE: Rec<'static> = Rec::Plain("ObjectTypeDefinition");
const OBJECT_FIELD: Rec<'static> = Rec::Th("ObjectFieldDefinition");
const RELATIONSHIP: Rec<'static> = Rec::Plain("TypeRelationshipDefinition");
const SCALAR: Rec<'static> = Rec::Th("ScalarTypeDefinition");
const ENUM: Rec<'static> = Rec::Th("EnumTypeDefinition");
const ENUM_VALUE: Rec<'static> = Rec::Th("EnumValueDefinition");

/// How V2 decodes a record, which decides its error messages.
#[derive(Clone, Copy)]
enum Rec<'a> {
    /// Hand-written `withObject "<label>"` with `.:`.
    Plain(&'a str),
    /// `genericParseJSON`: `parsing <Module.Type(Con)> failed, …`.
    Generic(&'a str),
    /// `deriveJSON` (Template Haskell) on a record of `Hasura.RQL.Types.CustomTypes`.
    Th(&'a str),
}

/// Parse an action definition at `$.args.definition` in the order of V2's `FromJSON`,
/// returning its stored form.
fn parse_definition(raw: &Value) -> ApiResult<(Value, ActionDefinition)> {
    const PATH: &str = "$.args.definition";
    let o = object(raw, PATH, Rec::Plain("ActionDefinition"))?;
    // The definition as V2 reads it: `null` keys absent, types normalised, the timeout
    // an integer.
    let mut norm: Map<String, Value> = o
        .iter()
        .filter(|(_, v)| !v.is_null())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if let Some(arguments) = optional(o, "arguments") {
        let path = child(PATH, "arguments");
        let mut normalised = vec![];
        for (i, a) in array(arguments, &path)?.iter().enumerate() {
            let p = index(&path, i);
            let ao = object(a, &p, ARGUMENT)?;
            let name = name(required(ao, "name", &p, ARGUMENT)?, &child(&p, "name"))?;
            let ty = graphql_type(required(ao, "type", &p, ARGUMENT)?, &child(&p, "type"))?;
            let description = opt_text(ao, "description", &p)?;
            normalised.push(json!({"name": name, "type": ty, "description": description}));
        }
        norm.insert("arguments".into(), Value::Array(normalised));
    }
    let output_type = graphql_type(
        required(o, "output_type", PATH, Rec::Plain(""))?,
        &child(PATH, "output_type"),
    )?;
    norm.insert("output_type".into(), Value::String(output_type));
    if let Some(headers) = optional(o, "headers") {
        let path = child(PATH, "headers");
        let mut normalised = vec![];
        for (i, h) in array(headers, &path)?.iter().enumerate() {
            normalised.push(header(h, &index(&path, i))?);
        }
        norm.insert("headers".into(), Value::Array(normalised));
    }
    opt_bool(o, "forward_client_headers", PATH)?;
    if let Some(ignored) = optional(o, "ignored_client_headers") {
        let path = child(PATH, "ignored_client_headers");
        for (i, h) in array(ignored, &path)?.iter().enumerate() {
            string(h, &index(&path, i), Some("Text"))?;
        }
    }
    let handler_path = child(PATH, "handler");
    let handler = string(
        required(o, "handler", PATH, Rec::Plain(""))?,
        &handler_path,
        Some("String"),
    )?;
    url_template(&handler, &handler_path)?;
    if let Some(t) = optional(o, "timeout") {
        norm.insert(
            "timeout".into(),
            json!(timeout(t, &child(PATH, "timeout"))?),
        );
    }
    let ty = match optional(o, "type") {
        None => "mutation".to_owned(),
        Some(t) => string(t, &child(PATH, "type"), None)?,
    };
    match ty.as_str() {
        "mutation" => {
            if let Some(kind) = optional(o, "kind") {
                let path = child(PATH, "kind");
                let kind = string(kind, &path, Some(MUTATION_KIND))?;
                if !matches!(kind.as_str(), "synchronous" | "asynchronous") {
                    return Err(parse_failed(
                        &path,
                        format!(
                            "parsing {MUTATION_KIND} failed, expected one of the tags [\"synchronous\",\"asynchronous\"], but found tag \"{kind}\""
                        ),
                    ));
                }
            }
        }
        "query" => {}
        other => {
            return Err(parse_failed(
                PATH,
                format!("expected mutation or query, but found {other}"),
            ));
        }
    }
    let transform_error = |key: &str, e: ParseError| {
        let rest = e.path.strip_prefix('$').unwrap_or(&e.path);
        parse_failed(&format!("{PATH}.{key}{rest}"), e.message)
    };
    if let Some(t) = optional(o, "request_transform") {
        RequestTransform::from_json(t).map_err(|e| transform_error("request_transform", e))?;
    }
    if let Some(t) = optional(o, "response_transform") {
        ResponseTransform::from_json(t).map_err(|e| transform_error("response_transform", e))?;
    }
    let norm = Value::Object(norm);
    let def = ActionDefinition::from_json(&strip_nulls(norm.clone()))
        .map_err(|e| parse_failed(PATH, e))?;
    let Value::Object(norm) = norm else {
        unreachable!("built as an object")
    };
    Ok((definition_json(&norm, &def), def))
}

/// V2's `HeaderConf` parser; returns the header without `null` keys.
fn header(h: &Value, p: &str) -> ApiResult<Value> {
    let Some(o) = h.as_object() else {
        return Err(parse_failed(p, "expecting object for headers"));
    };
    let name = string(
        required(o, "name", p, Rec::Plain(""))?,
        &child(p, "name"),
        Some("Text"),
    )?;
    let value = optional(o, "value");
    let env = opt_text(o, "value_from_env", p)?;
    match (value, env) {
        (None, None) => Err(parse_failed(p, "expecting value or value_from_env keys")),
        (Some(v), None) => {
            let value = string(v, p, Some("Template"))?;
            url_template(&value, p)?;
            Ok(json!({"name": name, "value": value}))
        }
        (None, Some(env)) => {
            if env.starts_with("HASURA_GRAPHQL_") {
                return Err(parse_failed(
                    p,
                    format!(
                        "env variables starting with \"HASURA_GRAPHQL_\" are not allowed in value_from_env: {env}"
                    ),
                ));
            }
            Ok(json!({"name": name, "value_from_env": env}))
        }
        (Some(_), Some(_)) => Err(parse_failed(
            p,
            "expecting only one of value or value_from_env keys",
        )),
    }
}

/// `{{VAR}}` templates (`Data.URL.Template`), checked when the metadata is parsed.
fn url_template(s: &str, path: &str) -> ApiResult<()> {
    hge_actions::env::Template::parse(s)
        .map(|_| ())
        .map_err(|e| parse_failed(path, format!("Parsing URL template failed: {e}")))
}

/// V2's `Timeout` parser (`toBoundedInteger`, then not negative).
fn timeout(v: &Value, path: &str) -> ApiResult<u64> {
    let Value::Number(n) = v else {
        return Err(kind_error(path, Some("Timeout"), "Number", v));
    };
    let integral = n.as_i64().or_else(|| {
        n.as_f64()
            .filter(|f| f.fract() == 0.0 && *f >= i64::MIN as f64 && *f <= i64::MAX as f64)
            .map(|f| f as i64)
    });
    match integral {
        Some(t) if t < 0 => Err(parse_failed(path, "timeout value cannot be negative")),
        Some(t) => Ok(t.unsigned_abs()),
        None => Err(parse_failed(path, format!("{n} is out of bounds"))),
    }
}

/// V2's serialisation of a parsed definition (`actionDefinitionToOrdJSON`): defaults are
/// written out (`ignored_client_headers`, `type`, `kind`), `timeout` only when not 30.
fn definition_json(raw: &Map<String, Value>, d: &ActionDefinition) -> Value {
    let mut o = Map::new();
    o.insert("handler".into(), Value::String(d.handler.clone()));
    o.insert("output_type".into(), Value::String(d.output_type.clone()));
    if d.forward_client_headers {
        o.insert("forward_client_headers".into(), Value::Bool(true));
    }
    o.insert(
        "ignored_client_headers".into(),
        json!(d.ignored_client_headers),
    );
    if !d.headers.is_empty() {
        let headers = d
            .headers
            .iter()
            .map(|h| match h {
                HeaderConf::Value { name, value } => json!({"name": name, "value": value}),
                HeaderConf::FromEnv { name, env } => json!({"name": name, "value_from_env": env}),
            })
            .collect();
        o.insert("headers".into(), Value::Array(headers));
    }
    if !d.arguments.is_empty() {
        let arguments = d
            .arguments
            .iter()
            .map(|a| {
                let mut arg = Map::new();
                arg.insert("name".into(), Value::String(a.name.clone()));
                arg.insert("type".into(), Value::String(a.ty.clone()));
                described(&mut arg, &a.description);
                Value::Object(arg)
            })
            .collect();
        o.insert("arguments".into(), Value::Array(arguments));
    }
    for key in ["request_transform", "response_transform"] {
        if let Some(t) = optional(raw, key) {
            o.insert(key.into(), t.clone());
        }
    }
    match d.action_type {
        ActionType::Query => {
            o.insert("type".into(), json!("query"));
        }
        ActionType::Mutation(kind) => {
            o.insert("type".into(), json!("mutation"));
            let kind = match kind {
                ActionKind::Synchronous => "synchronous",
                ActionKind::Asynchronous => "asynchronous",
            };
            o.insert("kind".into(), json!(kind));
        }
    }
    if d.timeout_secs != DEFAULT_ACTION_TIMEOUT_SECS {
        o.insert("timeout".into(), json!(d.timeout_secs));
    }
    Value::Object(o)
}

/// Parse `set_custom_types` arguments in the order of V2's `FromJSON` instances, into
/// their stored form (`None` when empty: V2 omits empty custom types from the metadata).
fn parse_custom_types(args: &Value) -> ApiResult<Option<Value>> {
    let o = object(args, ARGS, Rec::Plain("CustomTypes"))?;
    let list = |key: &str| -> ApiResult<Vec<(String, &Value)>> {
        let path = child(ARGS, key);
        Ok(match optional(o, key) {
            None => vec![],
            Some(v) => array(v, &path)?
                .iter()
                .enumerate()
                .map(|(i, t)| (index(&path, i), t))
                .collect(),
        })
    };
    for (p, t) in list("input_objects")? {
        let to = object(t, &p, INPUT_OBJECT)?;
        name(required(to, "name", &p, INPUT_OBJECT)?, &child(&p, "name"))?;
        opt_text(to, "description", &p)?;
        let fields = child(&p, "fields");
        let list = non_empty(required(to, "fields", &p, INPUT_OBJECT)?, &fields)?;
        for (i, f) in list.iter().enumerate() {
            let fp = index(&fields, i);
            let fo = object(f, &fp, INPUT_FIELD)?;
            name(required(fo, "name", &fp, INPUT_FIELD)?, &child(&fp, "name"))?;
            opt_text(fo, "description", &fp)?;
            graphql_type(required(fo, "type", &fp, INPUT_FIELD)?, &child(&fp, "type"))?;
        }
    }
    for (p, t) in list("objects")? {
        let to = object(t, &p, OBJECT_TYPE)?;
        name(required(to, "name", &p, OBJECT_TYPE)?, &child(&p, "name"))?;
        opt_text(to, "description", &p)?;
        let fields = child(&p, "fields");
        for (i, f) in non_empty(required(to, "fields", &p, OBJECT_TYPE)?, &fields)?
            .iter()
            .enumerate()
        {
            let fp = index(&fields, i);
            let fo = object(f, &fp, OBJECT_FIELD)?;
            name(
                required(fo, "name", &fp, OBJECT_FIELD)?,
                &child(&fp, "name"),
            )?;
            opt_text(fo, "description", &fp)?;
            graphql_type(
                required(fo, "type", &fp, OBJECT_FIELD)?,
                &child(&fp, "type"),
            )?;
        }
        if let Some(rels) = optional(to, "relationships") {
            let path = child(&p, "relationships");
            for (i, r) in array(rels, &path)?.iter().enumerate() {
                relationship(r, &index(&path, i))?;
            }
        }
    }
    for (p, t) in list("scalars")? {
        let to = object(t, &p, SCALAR)?;
        name(required(to, "name", &p, SCALAR)?, &child(&p, "name"))?;
        opt_text(to, "description", &p)?;
    }
    for (p, t) in list("enums")? {
        let to = object(t, &p, ENUM)?;
        name(required(to, "name", &p, ENUM)?, &child(&p, "name"))?;
        opt_text(to, "description", &p)?;
        let values = child(&p, "values");
        for (i, v) in non_empty(required(to, "values", &p, ENUM)?, &values)?
            .iter()
            .enumerate()
        {
            let vp = index(&values, i);
            let vo = object(v, &vp, ENUM_VALUE)?;
            name(
                required(vo, "value", &vp, ENUM_VALUE)?,
                &child(&vp, "value"),
            )?;
            opt_text(vo, "description", &vp)?;
            opt_bool(vo, "is_deprecated", &vp)?;
        }
    }
    let ct = CustomTypes::from_json(Some(args)).map_err(|e| parse_failed(ARGS, e))?;
    Ok(custom_types_json(&ct))
}

/// `TypeRelationshipDefinition` (`withObject`): name, type, source, remote_table,
/// field_mapping.
fn relationship(r: &Value, p: &str) -> ApiResult<()> {
    let ro = object(r, p, RELATIONSHIP)?;
    name(required(ro, "name", p, RELATIONSHIP)?, &child(p, "name"))?;
    let ty = required(ro, "type", p, RELATIONSHIP)?;
    if !matches!(ty.as_str(), Some("object" | "array")) {
        return Err(parse_failed(
            &child(p, "type"),
            "expecting either 'object' or 'array' for rel_type",
        ));
    }
    if let Some(source) = optional(ro, "source") {
        let path = child(p, "source");
        // `sourceNameParser`: "default", or a `NonEmptyText`.
        if string(source, &path, Some("String"))?.is_empty() {
            return Err(parse_failed(&path, "empty string not allowed"));
        }
    }
    let table_path = child(p, "remote_table");
    match required(ro, "remote_table", p, RELATIONSHIP)? {
        Value::String(_) => {}
        Value::Object(t) => {
            opt_text(t, "schema", &table_path)?;
            string(
                required(t, "name", &table_path, Rec::Plain(""))?,
                &child(&table_path, "name"),
                Some("Text"),
            )?;
        }
        _ => {
            return Err(parse_failed(
                &table_path,
                "expecting a string/object for QualifiedObject",
            ));
        }
    }
    let mapping_path = child(p, "field_mapping");
    let mapping = object(
        required(ro, "field_mapping", p, RELATIONSHIP)?,
        &mapping_path,
        Rec::Plain("HashMap"),
    )?;
    for (field, column) in mapping {
        let fp = child(&mapping_path, field);
        if !is_graphql_name(field) {
            return Err(parse_failed(
                &fp,
                format!("{field} is not valid GraphQL name"),
            ));
        }
        string(column, &fp, Some("Text"))?;
    }
    Ok(())
}

/// V2's serialisation of custom types (`customTypesToOrdJSON`). Enum values come out of
/// their plain `ToJSON`, unset fields as `null`.
fn custom_types_json(ct: &CustomTypes) -> Option<Value> {
    fn named(name: &str, description: &Option<String>) -> Map<String, Value> {
        let mut o = Map::new();
        o.insert("name".into(), Value::String(name.to_owned()));
        described(&mut o, description);
        o
    }
    fn field(name: &str, ty: &str, description: &Option<String>) -> Map<String, Value> {
        let mut o = named(name, description);
        o.insert("type".into(), Value::String(ty.to_owned()));
        o
    }
    let input_objects = ct.input_objects.iter().map(|t| {
        let mut o = named(&t.name, &t.description);
        let fields = t
            .fields
            .iter()
            .map(|f| Value::Object(field(&f.name, &stored_type(&f.ty), &f.description)))
            .collect();
        o.insert("fields".into(), Value::Array(fields));
        Value::Object(o)
    });
    let objects = ct.objects.iter().map(|t| {
        let mut o = named(&t.name, &t.description);
        let fields = t
            .fields
            .iter()
            .map(|f| {
                let mut fo = field(&f.name, &stored_type(&f.ty), &f.description);
                if let Some(a) = f.arguments.as_ref().filter(|a| !a.is_null()) {
                    fo.insert("arguments".into(), a.clone());
                }
                Value::Object(fo)
            })
            .collect();
        o.insert("fields".into(), Value::Array(fields));
        if !t.relationships.is_empty() {
            let rels = t
                .relationships
                .iter()
                .map(|r| {
                    let ty = match r.rel_type {
                        RelType::Object => "object",
                        RelType::Array => "array",
                    };
                    json!({
                        "name": r.name,
                        "type": ty,
                        "source": r.source,
                        "remote_table": {"schema": r.remote_table.schema, "name": r.remote_table.name},
                        "field_mapping": r.field_mapping,
                    })
                })
                .collect();
            o.insert("relationships".into(), Value::Array(rels));
        }
        Value::Object(o)
    });
    let scalars = ct
        .scalars
        .iter()
        .map(|t| Value::Object(named(&t.name, &t.description)));
    let enums = ct.enums.iter().map(|t| {
        let mut o = named(&t.name, &t.description);
        let values = t
            .values
            .iter()
            .map(|v| json!({"value": v.value, "description": v.description, "is_deprecated": v.is_deprecated}))
            .collect();
        o.insert("values".into(), Value::Array(values));
        Value::Object(o)
    });
    let mut out = Map::new();
    for (key, items) in [
        ("input_objects", input_objects.collect::<Vec<_>>()),
        ("objects", objects.collect()),
        ("scalars", scalars.collect()),
        ("enums", enums.collect()),
    ] {
        if !items.is_empty() {
            out.insert(key.into(), Value::Array(items));
        }
    }
    (!out.is_empty()).then_some(Value::Object(out))
}

/// A type string as stored: normalised (checked by the parse already).
fn stored_type(ty: &str) -> String {
    normalise_type(ty).unwrap_or_else(|| ty.to_owned())
}

fn described(o: &mut Map<String, Value>, description: &Option<String>) {
    if let Some(d) = description {
        o.insert("description".into(), Value::String(d.clone()));
    }
}

// Decoding helpers with aeson's messages and paths.

/// `path.key`, or `path["key"]` when the key is not an identifier (aeson's `JSONPath`).
fn child(path: &str, key: &str) -> String {
    let identifier = key
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if identifier {
        format!("{path}.{key}")
    } else {
        format!("{path}[{}]", Value::String(key.to_owned()))
    }
}

fn index(path: &str, i: usize) -> String {
    format!("{path}[{i}]")
}

/// `[parsing <parser> failed, ]expected <expected>, but encountered <kind>`; aeson's
/// `typeMismatch` for some parsers (`Bool`, `String`) has no prefix.
fn kind_error(path: &str, parser: Option<&str>, expected: &str, v: &Value) -> ApiError {
    let mismatch = format!("expected {expected}, but encountered {}", json_kind(v));
    parse_failed(
        path,
        match parser {
            Some(p) => format!("parsing {p} failed, {mismatch}"),
            None => mismatch,
        },
    )
}

/// An object for the record `rec`.
fn object<'a>(v: &'a Value, path: &str, rec: Rec<'_>) -> ApiResult<&'a Map<String, Value>> {
    v.as_object().ok_or_else(|| match rec {
        Rec::Plain(label) | Rec::Generic(label) => kind_error(path, Some(label), "Object", v),
        Rec::Th(con) => parse_failed(
            path,
            format!(
                "When parsing the constructor {con} of type Hasura.RQL.Types.CustomTypes.{con} expected Object but got {}.",
                json_kind(v)
            ),
        ),
    })
}

/// The arguments object of a generically decoded V2 record.
fn record<'a>(args: &'a Value, record_name: &str) -> ApiResult<&'a Map<String, Value>> {
    object(args, ARGS, Rec::Generic(record_name))
}

/// A key the record `rec` requires.
fn required<'a>(
    o: &'a Map<String, Value>,
    key: &str,
    path: &str,
    rec: Rec<'_>,
) -> ApiResult<&'a Value> {
    o.get(key).ok_or_else(|| {
        parse_failed(
            path,
            match rec {
                Rec::Plain(_) => format!("key \"{key}\" not found"),
                Rec::Generic(r) => format!("parsing {r} failed, key \"{key}\" not found"),
                Rec::Th(con) => format!(
                    "When parsing the record {con} of type Hasura.RQL.Types.CustomTypes.{con} the key {key} was not present."
                ),
            },
        )
    })
}

/// A key `.:?` reads: absent and `null` are both unset.
fn optional<'a>(o: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    o.get(key).filter(|v| !v.is_null())
}

fn array<'a>(v: &'a Value, path: &str) -> ApiResult<&'a [Value]> {
    v.as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| kind_error(path, Some("[]"), "Array", v))
}

fn non_empty<'a>(v: &'a Value, path: &str) -> ApiResult<&'a [Value]> {
    match v.as_array() {
        None => Err(kind_error(path, Some("NonEmpty"), "Array", v)),
        Some(items) if items.is_empty() => Err(parse_failed(
            path,
            "parsing NonEmpty failed, unexpected empty list",
        )),
        Some(items) => Ok(items),
    }
}

fn string(v: &Value, path: &str, parser: Option<&str>) -> ApiResult<String> {
    v.as_str()
        .map(str::to_owned)
        .ok_or_else(|| kind_error(path, parser, "String", v))
}

/// A GraphQL name (`G.Name`).
fn name(v: &Value, path: &str) -> ApiResult<String> {
    let name = string(v, path, Some("Name"))?;
    if !is_graphql_name(&name) {
        return Err(parse_failed(
            path,
            format!("{name} is not valid GraphQL name"),
        ));
    }
    Ok(name)
}

/// A GraphQL type (`GraphQLType`), normalised as V2 prints it.
fn graphql_type(v: &Value, path: &str) -> ApiResult<String> {
    let ty = string(v, path, Some("GraphQLType"))?;
    normalise_type(&ty).ok_or_else(|| parse_failed(path, format!("not a valid GraphQL type: {ty}")))
}

fn opt_text(o: &Map<String, Value>, key: &str, path: &str) -> ApiResult<Option<String>> {
    optional(o, key)
        .map(|v| string(v, &child(path, key), Some("Text")))
        .transpose()
}

fn opt_bool(o: &Map<String, Value>, key: &str, path: &str) -> ApiResult<Option<bool>> {
    optional(o, key)
        .map(|v| {
            v.as_bool()
                .ok_or_else(|| kind_error(&child(path, key), None, "Bool", v))
        })
        .transpose()
}

/// A required GraphQL name (`ActionName`) of the arguments.
fn graphql_name(obj: &Map<String, Value>, key: &str, record_name: &str) -> ApiResult<String> {
    name(
        required(obj, key, ARGS, Rec::Generic(record_name))?,
        &child(ARGS, key),
    )
}

/// A required role name (`NonEmptyText`) of the arguments.
fn role_name(obj: &Map<String, Value>, key: &str, record_name: &str) -> ApiResult<String> {
    let path = child(ARGS, key);
    let role = string(
        required(obj, key, ARGS, Rec::Generic(record_name))?,
        &path,
        Some("String"),
    )?;
    if role.is_empty() {
        return Err(parse_failed(&path, "empty string not allowed"));
    }
    Ok(role)
}

/// An optional text of the arguments.
fn optional_text(obj: &Map<String, Value>, key: &str) -> ApiResult<Option<String>> {
    opt_text(obj, key, ARGS)
}

/// `/[_A-Za-z][_0-9A-Za-z]*/`
fn is_graphql_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// graphql-parser's `parseGraphQLType`, printed back as V2's `GraphQLType` `ToJSON` does
/// (`[ Int ]` → `[Int]`). No leading whitespace; after each token whitespace (space, tab,
/// CR, LF, comma) and `#` comments are skipped. `None` if it does not parse.
fn normalise_type(s: &str) -> Option<String> {
    fn skip(mut s: &str) -> &str {
        loop {
            let t = s.trim_start_matches([' ', '\t', '\n', '\r', ',']);
            match t.strip_prefix('#') {
                Some(comment) => {
                    s = comment.find(['\n', '\r']).map_or("", |end| &comment[end..]);
                }
                None => return t,
            }
        }
    }
    fn ty<'a>(s: &'a str, out: &mut String) -> Option<&'a str> {
        let rest = if let Some(inner) = s.strip_prefix('[') {
            out.push('[');
            let rest = skip(ty(skip(inner), out)?).strip_prefix(']')?;
            out.push(']');
            rest
        } else {
            let end = s
                .find(|c: char| !(c == '_' || c.is_ascii_alphanumeric()))
                .unwrap_or(s.len());
            if !is_graphql_name(&s[..end]) {
                return None;
            }
            out.push_str(&s[..end]);
            &s[end..]
        };
        let rest = skip(rest);
        Some(match rest.strip_prefix('!') {
            Some(r) => {
                out.push('!');
                skip(r)
            }
            None => rest,
        })
    }
    let mut out = String::new();
    ty(s, &mut out).filter(|rest| rest.is_empty())?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn errors_are_400() {
        let e = test_webhook_transform(&json!({
            "webhook_url": {"from_env": "NOPE"}, "body": {}, "request_transform": {}
        }))
        .unwrap_err();
        assert_eq!(
            (e.status, e.error.code, e.error.path.as_str()),
            (400, hge_core::Code::NotFound, "$.args")
        );
        let ok = test_webhook_transform(&json!({
            "webhook_url": "http://localhost:1234", "body": {"a": 1},
            "request_transform": {"method": "PUT"}
        }))
        .unwrap();
        assert_eq!(ok["method"], "PUT");
    }

    #[test]
    fn definitions_are_stored_like_v2_serialises_them() {
        let (stored, _) = parse_definition(&json!({
            "handler": "{{BASE}}/x", "output_type": "Out", "unknown": 1,
            "arguments": [{"name": "a", "type": "Int!", "description": null}],
            "headers": [{"name": "X", "value_from_env": "E"}], "timeout": 30
        }))
        .unwrap();
        assert_eq!(
            stored,
            json!({
                "handler": "{{BASE}}/x", "output_type": "Out",
                "ignored_client_headers": hge_metadata::actions::DEFAULT_IGNORED_CLIENT_HEADERS,
                "headers": [{"name": "X", "value_from_env": "E"}],
                "arguments": [{"name": "a", "type": "Int!"}],
                "type": "mutation", "kind": "synchronous"
            })
        );
        let (query, _) = parse_definition(&json!({
            "type": "query", "kind": "", "handler": "h", "output_type": "O",
            "forward_client_headers": true, "ignored_client_headers": [], "timeout": 5
        }))
        .unwrap();
        assert_eq!(
            query,
            json!({"handler": "h", "output_type": "O", "forward_client_headers": true,
                   "ignored_client_headers": [], "type": "query", "timeout": 5})
        );
    }

    #[test]
    fn definition_errors_have_paths() {
        let e = parse_definition(&json!({"handler": "h"})).unwrap_err();
        assert_eq!(
            (e.error.path.as_str(), e.error.message.as_str()),
            ("$.args.definition", "key \"output_type\" not found")
        );
        let e = parse_definition(&json!({"handler": "h", "output_type": "O",
            "headers": [{"name": "a", "value": "1"}, {"name": "b", "value_from_env": "HASURA_GRAPHQL_X"}]}))
        .unwrap_err();
        assert_eq!(e.error.path, "$.args.definition.headers[1]");
        let e = parse_definition(&json!("x")).unwrap_err();
        assert_eq!(
            e.error.message,
            "parsing ActionDefinition failed, expected Object, but encountered String"
        );
        let e = action_args(
            &json!({"name": "1x", "definition": {"handler": "h", "output_type": "O"}}),
            "CreateAction",
        )
        .unwrap_err();
        assert_eq!(
            (e.error.path.as_str(), e.error.message.as_str()),
            ("$.args.name", "1x is not valid GraphQL name")
        );
        let e = action_args(&json!({"name": "x"}), "UpdateAction").unwrap_err();
        assert_eq!(
            e.error.message,
            "parsing Hasura.RQL.DDL.Action.UpdateAction(UpdateAction) failed, key \"definition\" not found"
        );
        let e = action_args(
            &json!({"name": "x", "definition": {"handler": "h", "output_type": "O",
                "request_transform": {"template_engine": "Kriti", "url": "{{$x"}}}),
            "CreateAction",
        )
        .unwrap_err();
        assert_eq!(e.error.code, Code::ValidationFailed);
    }

    #[test]
    fn custom_types_are_normalised() {
        assert_eq!(parse_custom_types(&json!({})).unwrap(), None);
        assert_eq!(
            parse_custom_types(&json!({"objects": [], "scalars": []})).unwrap(),
            None
        );
        let ct = parse_custom_types(&json!({
            "objects": [{"name": "O", "description": null, "x": 1,
                "fields": [{"name": "id", "type": "Int!", "arguments": null}],
                "relationships": [{"name": "r", "type": "object", "remote_table": "t",
                                   "field_mapping": {"id": "id"}}]}],
            "enums": [{"name": "E", "values": [{"value": "A", "is_deprecated": false}]}]
        }))
        .unwrap()
        .unwrap();
        assert_eq!(
            ct,
            json!({
                "objects": [{"name": "O", "fields": [{"name": "id", "type": "Int!"}],
                    "relationships": [{"name": "r", "type": "object", "source": "default",
                        "remote_table": {"schema": "public", "name": "t"},
                        "field_mapping": {"id": "id"}}]}],
                "enums": [{"name": "E", "values": [{"value": "A", "description": null, "is_deprecated": false}]}]
            })
        );
        let e = parse_custom_types(&json!({"enums": [{"name": "E", "values": []}]})).unwrap_err();
        assert_eq!(e.error.path, "$.args.enums[0].values");
        let e = parse_custom_types(&json!([])).unwrap_err();
        assert_eq!(e.error.code, Code::ParseFailed);
    }

    #[test]
    fn aeson_messages_follow_the_v2_decoder() {
        let e = |p: &str, m: &str| (p.to_owned(), m.to_owned());
        let ct = |v: Value| parse_error(parse_custom_types(&v));
        // Template Haskell records of `Hasura.RQL.Types.CustomTypes`.
        assert_eq!(
            ct(json!({"input_objects": [{"name": "I"}]})),
            e(
                "$.args.input_objects[0]",
                "When parsing the record InputObjectTypeDefinition of type Hasura.RQL.Types.CustomTypes.InputObjectTypeDefinition the key fields was not present."
            )
        );
        assert_eq!(
            ct(json!({"input_objects": ["I"]})),
            e(
                "$.args.input_objects[0]",
                "When parsing the constructor InputObjectTypeDefinition of type Hasura.RQL.Types.CustomTypes.InputObjectTypeDefinition expected Object but got String."
            )
        );
        assert_eq!(
            ct(
                json!({"enums": [{"name": "E", "values": [{"value": "A", "is_deprecated": "no"}]}]})
            ),
            e(
                "$.args.enums[0].values[0].is_deprecated",
                "expected Bool, but encountered String"
            )
        );
        // `null` is absent.
        assert_eq!(
            parse_custom_types(&json!({"objects": null, "scalars": [{"name": "S"}]}))
                .unwrap()
                .unwrap(),
            json!({"scalars": [{"name": "S"}]})
        );
        let o = |rel: Value| {
            json!({"objects": [{"name": "O", "fields": [{"name": "id", "type": "[ Int ]"}],
                "relationships": rel}]})
        };
        let stored = parse_custom_types(&o(Value::Null)).unwrap().unwrap();
        assert_eq!(stored["objects"][0]["fields"][0]["type"], "[Int]");
        let rel = |extra: Value| {
            let mut r = json!({"name": "r", "type": "object", "remote_table": "t",
                "field_mapping": {"id": "id"}});
            r.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            o(json!([r]))
        };
        assert!(parse_custom_types(&rel(json!({"source": null}))).is_ok());
        assert_eq!(
            ct(rel(json!({"source": ""}))),
            e(
                "$.args.objects[0].relationships[0].source",
                "empty string not allowed"
            )
        );
        assert_eq!(
            ct(rel(json!({"type": 1}))),
            e(
                "$.args.objects[0].relationships[0].type",
                "expecting either 'object' or 'array' for rel_type"
            )
        );
        assert_eq!(
            ct(rel(json!({"field_mapping": {"a-b": "id"}}))),
            e(
                "$.args.objects[0].relationships[0].field_mapping[\"a-b\"]",
                "a-b is not valid GraphQL name"
            )
        );
        assert_eq!(
            ct(rel(json!({"field_mapping": []}))),
            e(
                "$.args.objects[0].relationships[0].field_mapping",
                "parsing HashMap failed, expected Object, but encountered Array"
            )
        );

        let def = |extra: Value| {
            let mut d = json!({"output_type": "O", "handler": "h"});
            d.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            parse_definition(&d)
        };
        assert_eq!(
            parse_error(def(json!({"forward_client_headers": "yes"}))),
            e(
                "$.args.definition.forward_client_headers",
                "expected Bool, but encountered String"
            )
        );
        assert_eq!(
            parse_error(def(json!({"type": 1}))),
            e(
                "$.args.definition.type",
                "expected String, but encountered Number"
            )
        );
        assert_eq!(
            parse_error(def(json!({"handler": "http://{{X"}))),
            e(
                "$.args.definition.handler",
                "Parsing URL template failed: unclosed '{{' in template: http://{{X"
            )
        );
        assert_eq!(
            parse_error(def(json!({"headers": [{"name": "a", "value": "{{X"}]}))),
            e(
                "$.args.definition.headers[0]",
                "Parsing URL template failed: unclosed '{{' in template: {{X"
            )
        );
        assert_eq!(
            parse_error(def(json!({"headers": [{"name": 1, "value": "v"}]}))),
            e(
                "$.args.definition.headers[0].name",
                "parsing Text failed, expected String, but encountered Number"
            )
        );
        let (stored, _) = def(json!({
            "arguments": [{"name": "a", "type": "[ Int ]", "description": null}],
            "headers": [{"name": "a", "value": null, "value_from_env": "X"}],
            "timeout": 1e3, "comment": null, "kind": null,
        }))
        .unwrap();
        assert_eq!(stored["arguments"], json!([{"name": "a", "type": "[Int]"}]));
        assert_eq!(
            stored["headers"],
            json!([{"name": "a", "value_from_env": "X"}])
        );
        assert_eq!(stored["timeout"], 1000);
        assert_eq!(stored["kind"], "synchronous");

        let e2 = drop_args(&json!({"name": "a", "clear_data": "no"})).unwrap_err();
        assert_eq!(e2.error.message, "expected Bool, but encountered String");
    }

    fn parse_error(r: ApiResult<impl std::fmt::Debug>) -> (String, String) {
        let e = r.unwrap_err();
        assert_eq!(e.error.code, Code::ParseFailed);
        (e.error.path, e.error.message)
    }

    #[test]
    fn names_and_types_are_checked_where_v2_checks_them() {
        let def = |extra: Value| {
            let mut d = json!({"output_type": "O", "handler": "h"});
            d.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            parse_definition(&d)
        };
        let e = |path: &str, message: &str| (path.to_owned(), message.to_owned());
        assert_eq!(
            parse_error(def(json!({"arguments": [{"name": "a-b", "type": "Int"}]}))),
            e(
                "$.args.definition.arguments[0].name",
                "a-b is not valid GraphQL name"
            )
        );
        assert_eq!(
            parse_error(def(
                json!({"arguments": [{"name": "a", "type": "Int", "description": 5}]})
            )),
            e(
                "$.args.definition.arguments[0].description",
                "parsing Text failed, expected String, but encountered Number"
            )
        );
        assert_eq!(
            parse_error(def(json!({"output_type": "Int!!"}))),
            e(
                "$.args.definition.output_type",
                "not a valid GraphQL type: Int!!"
            )
        );
        // V2's order: arguments before output_type, handler before timeout before type.
        assert_eq!(
            parse_error(def(json!({"arguments": 1, "output_type": 2}))).0,
            "$.args.definition.arguments"
        );
        assert_eq!(
            parse_error(def(json!({"timeout": -1, "type": "x"}))),
            e(
                "$.args.definition.timeout",
                "timeout value cannot be negative"
            )
        );
        assert_eq!(
            parse_error(def(json!({"type": "x"}))),
            e(
                "$.args.definition",
                "expected mutation or query, but found x"
            )
        );
        assert!(def(json!({"output_type": "[ [Int!] ]! "})).is_ok());
        assert!(def(json!({"output_type": " Int"})).is_err());

        let ct = |v: Value| parse_error(parse_custom_types(&v));
        assert_eq!(
            ct(json!({"objects": [{"name": "O"}]})),
            e("$.args.objects[0]", "key \"fields\" not found")
        );
        assert_eq!(
            ct(json!({"objects": "x"})),
            e(
                "$.args.objects",
                "parsing [] failed, expected Array, but encountered String"
            )
        );
        assert_eq!(
            ct(json!({"objects": [{"name": "O", "fields": [{"name": "f", "type": 1}]}]})),
            e(
                "$.args.objects[0].fields[0].type",
                "parsing GraphQLType failed, expected String, but encountered Number"
            )
        );
        assert_eq!(
            ct(json!({"scalars": [{"name": "1x"}]})),
            e("$.args.scalars[0].name", "1x is not valid GraphQL name")
        );
        assert_eq!(
            ct(json!({"enums": [{"name": "E", "values": [{"value": "a b"}]}]})),
            e(
                "$.args.enums[0].values[0].value",
                "a b is not valid GraphQL name"
            )
        );
        assert_eq!(
            ct(
                json!({"input_objects": [{"name": "I", "fields": [{"name": "f", "type": "[Int"}]}]})
            ),
            e(
                "$.args.input_objects[0].fields[0].type",
                "not a valid GraphQL type: [Int"
            )
        );
        // Left to right: an invalid name comes before the empty field list.
        assert_eq!(
            ct(json!({"input_objects": [{"name": "-", "fields": []}]})).0,
            "$.args.input_objects[0].name"
        );
        assert_eq!(
            ct(
                json!({"objects": [{"name": "O", "fields": [{"name": "f", "type": "Int"}],
                "relationships": [{"name": "r", "type": "x"}]}]})
            ),
            e(
                "$.args.objects[0].relationships[0].type",
                "expecting either 'object' or 'array' for rel_type"
            )
        );
    }

    #[test]
    fn graphql_types() {
        for (ok, normal) in [
            ("Int", "Int"),
            ("Int!", "Int!"),
            ("[Int]", "[Int]"),
            ("[[Int!]!]!", "[[Int!]!]!"),
            ("[ Int ] ", "[Int]"),
            ("[ Int , ]\t", "[Int]"),
            ("[Int # c\n]!", "[Int]!"),
            ("Int ! ", "Int!"),
        ] {
            assert_eq!(normalise_type(ok).as_deref(), Some(normal), "{ok}");
        }
        for bad in [
            "",
            " Int",
            "\nInt",
            "Int!!",
            "[Int",
            "Int]",
            "[]",
            "1x",
            "[Int]!!",
            "Int Int",
            "[ Int ]\u{a0}",
        ] {
            assert_eq!(normalise_type(bad), None, "{bad}");
        }
    }

    #[test]
    fn permission_lookup() {
        let actions = vec![json!({"name": "a", "permissions": [{"role": "user"}]})];
        assert!(has_permission(&actions, "a", "user"));
        assert!(!has_permission(&actions, "a", "admin"));
        assert!(!has_permission(&actions, "b", "user"));
        assert!(is_graphql_name("_a1") && !is_graphql_name("a-b") && !is_graphql_name(""));
    }
}
