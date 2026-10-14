//! Table relationships and permissions: `pg_create_object_relationship`,
//! `pg_create_array_relationship`, `pg_drop_relationship`, `pg_rename_relationship`,
//! `pg_set_relationship_comment`, `pg_create_*_permission`, `pg_drop_*_permission`,
//! `pg_set_permission_comment` and `pg_suggest_relationships`.

use std::collections::HashSet;

use hge_core::{Code, QualifiedTable, SourceCatalog};
use hge_metadata::{
    ArrayRelUsing, ArrayRelationship, DeletePermission, InsertPermission, ObjectFkOn,
    ObjectRelUsing, ObjectRelationship, Permission, SelectPermission, Source, TableEntry,
    UpdatePermission,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::sources_tables::{
    Args, catalog, dependency_error, drop_dependents, parse_table, relationship_dependents,
    relationship_target, source_ref, table_item_name, table_mut, table_ref,
};
use crate::ctx::{Check, Ctx};
use crate::response::{ApiError, ApiResult, already_exists, not_exists, parse_failed, success};
use crate::runtime::Runtime;

/// Commands this module implements (`pg_` prefix stripped).
pub const COMMANDS: &[&str] = &[
    "create_object_relationship",
    "create_array_relationship",
    "drop_relationship",
    "rename_relationship",
    "set_relationship_comment",
    "create_insert_permission",
    "create_select_permission",
    "create_update_permission",
    "create_delete_permission",
    "drop_insert_permission",
    "drop_select_permission",
    "drop_update_permission",
    "drop_delete_permission",
    "set_permission_comment",
    "suggest_relationships",
];

pub async fn run<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    command: &str,
    args: &Value,
) -> ApiResult<Value> {
    match command {
        "create_object_relationship" => create_relationship(ctx, args, RelKind::Object).await,
        "create_array_relationship" => create_relationship(ctx, args, RelKind::Array).await,
        "drop_relationship" => drop_relationship(ctx, args).await,
        "rename_relationship" => rename_relationship(ctx, args).await,
        "set_relationship_comment" => set_relationship_comment(ctx, args).await,
        "create_insert_permission" => create_permission(ctx, args, "insert").await,
        "create_select_permission" => create_permission(ctx, args, "select").await,
        "create_update_permission" => create_permission(ctx, args, "update").await,
        "create_delete_permission" => create_permission(ctx, args, "delete").await,
        "drop_insert_permission" => drop_permission(ctx, args, "insert").await,
        "drop_select_permission" => drop_permission(ctx, args, "select").await,
        "drop_update_permission" => drop_permission(ctx, args, "update").await,
        "drop_delete_permission" => drop_permission(ctx, args, "delete").await,
        "set_permission_comment" => set_permission_comment(ctx, args).await,
        "suggest_relationships" => suggest_relationships(ctx, args).await,
        _ => Err(super::not_implemented(command)),
    }
}

// ---------------------------------------------------------------------------------------
// Relationships

const REL_DEF: &str = "Hasura.RQL.Types.Relationships.Local.RelDef(RelDef)";
const PERM_DEF: &str = "Hasura.RQL.Types.Permission.PermDef(PermDef)";

/// aeson's `parsing <Type> failed, …` prefix for errors of a record's own fields.
fn prefixed(mut e: ApiError, ty: &str) -> ApiError {
    if e.error.path == "$.args" {
        e.error.message = format!("parsing {ty} failed, {}", e.error.message);
    }
    e
}

#[derive(Clone, Copy, PartialEq)]
enum RelKind {
    Object,
    Array,
}

/// Names of a table's fields (columns, relationships, computed fields, remote
/// relationships).
fn field_names(entry: &TableEntry, catalog: Option<&SourceCatalog>) -> HashSet<String> {
    let mut names: HashSet<String> = entry
        .object_relationships
        .iter()
        .map(|r| r.name.clone())
        .chain(entry.array_relationships.iter().map(|r| r.name.clone()))
        .collect();
    let named = |v: &Value| v.get("name").and_then(Value::as_str).map(str::to_owned);
    names.extend(entry.computed_fields.iter().filter_map(named));
    names.extend(entry.remote_relationships.iter().filter_map(named));
    if let Some(t) = catalog.and_then(|c| c.table(&entry.table)) {
        names.extend(t.columns.keys().cloned());
    }
    names
}

fn relationship_exists(entry: &TableEntry, name: &str) -> bool {
    entry.object_relationships.iter().any(|r| r.name == name)
        || entry.array_relationships.iter().any(|r| r.name == name)
}

fn relationship_not_found(name: &str) -> ApiError {
    not_exists(format!("relationship \"{name}\" does not exist"))
}

/// V2 stores a single-column foreign key as a plain column.
fn normalise_object_using(using: ObjectRelUsing) -> ObjectRelUsing {
    match using {
        ObjectRelUsing::ForeignKey {
            foreign_key_constraint_on: ObjectFkOn::Columns(cols),
        } if cols.len() == 1 => ObjectRelUsing::ForeignKey {
            foreign_key_constraint_on: ObjectFkOn::Column(
                cols.into_iter().next().unwrap_or_default(),
            ),
        },
        ObjectRelUsing::ForeignKey {
            foreign_key_constraint_on: ObjectFkOn::Remote(mut fk),
        } => {
            normalise_remote_fk(&mut fk);
            ObjectRelUsing::ForeignKey {
                foreign_key_constraint_on: ObjectFkOn::Remote(fk),
            }
        }
        other => other,
    }
}

fn normalise_remote_fk(fk: &mut hge_metadata::RemoteFk) {
    if let Some(cols) = &fk.columns
        && cols.len() == 1
    {
        fk.column = cols.first().cloned();
        fk.columns = None;
    }
}

/// The `using` of a relationship definition.
fn parse_using<T: DeserializeOwned>(a: &Args<'_>) -> ApiResult<T> {
    let Some(using) = a.raw("using") else {
        return Err(parse_failed(
            "$.args",
            format!("parsing {REL_DEF} failed, key \"using\" not found"),
        ));
    };
    let obj = Args::at(using, "$.args.using")?;
    match (
        obj.has("foreign_key_constraint_on"),
        obj.has("manual_configuration"),
    ) {
        (false, false) => {
            return Err(parse_failed(
                "$.args.using",
                "atleast one of foreign_key_constraint_on/manual_configuration should be present",
            ));
        }
        (true, true) => {
            return Err(parse_failed(
                "$.args.using",
                "only one of foreign_key_constraint_on/manual_configuration should be present",
            ));
        }
        _ => {}
    }
    // The untagged model types would only say that no variant matched.
    if let Some(Value::Object(fk)) = using.get("foreign_key_constraint_on")
        && fk.contains_key("table")
        && fk.get("column").is_some_and(|v| !v.is_null())
            == fk.get("columns").is_some_and(|v| !v.is_null())
    {
        return Err(parse_failed(
            "$.args.using",
            hge_metadata::REMOTE_FK_COLUMNS_ERROR,
        ));
    }
    serde_json::from_value(using.clone())
        .map_err(|e| parse_failed("$.args.using", crate::response::aeson_message(&e)))
}

async fn create_relationship<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
    kind: RelKind,
) -> ApiResult<Value> {
    let a = Args::new(args)?;
    let source = a.source()?;
    let table = a.table("table")?;
    let name = a.string("name").map_err(|e| prefixed(e, REL_DEF))?;
    let name = a.check_non_empty("name", name)?;
    let comment = a.opt_string("comment")?;
    let object_using = match kind {
        RelKind::Object => Some(normalise_object_using(parse_using::<ObjectRelUsing>(&a)?)),
        RelKind::Array => None,
    };
    let array_using = match kind {
        RelKind::Array => Some(match parse_using::<ArrayRelUsing>(&a)? {
            ArrayRelUsing::ForeignKey {
                mut foreign_key_constraint_on,
            } => {
                normalise_remote_fk(&mut foreign_key_constraint_on);
                ArrayRelUsing::ForeignKey {
                    foreign_key_constraint_on,
                }
            }
            other => other,
        }),
        RelKind::Object => None,
    };
    let cat = catalog(ctx, &source);
    let entry = table_ref(&ctx.metadata, &source, &table)?;
    if field_names(entry, cat.as_deref()).contains(&name) {
        return Err(already_exists(format!(
            "field with name \"{name}\" already exists in table \"{}\"",
            table.v2_text()
        )));
    }
    let entry = table_mut(&mut ctx.metadata, &source, &table)?;
    let kind_name = if let Some(using) = object_using {
        entry.object_relationships.push(ObjectRelationship {
            name: name.clone(),
            using,
            comment,
        });
        "object_relation"
    } else {
        entry.array_relationships.push(ArrayRelationship {
            name: name.clone(),
            using: array_using.expect("array relationship"),
            comment,
        });
        "array_relation"
    };
    ctx.check(Check::object(
        kind_name,
        table_item_name(kind_name, &name, &source, &table),
    ))
    .await?;
    Ok(success())
}

async fn drop_relationship<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let a = Args::new(args)?;
    let source = a.source()?;
    let table = a.table("table")?;
    let name = a.non_empty("relationship")?;
    let cascade = a.bool_or("cascade", false)?;
    let cat = catalog(ctx, &source);
    if !relationship_exists(table_ref(&ctx.metadata, &source, &table)?, &name) {
        return Err(relationship_not_found(&name));
    }
    let src = source_ref(&ctx.metadata, &source)?;
    let dependents = relationship_dependents(src, cat.as_deref(), &table, &name);
    if !dependents.is_empty() {
        if !cascade {
            return Err(dependency_error(&dependents));
        }
        drop_dependents(&mut ctx.metadata, &dependents);
    }
    let entry = table_mut(&mut ctx.metadata, &source, &table)?;
    entry.object_relationships.retain(|r| r.name != name);
    entry.array_relationships.retain(|r| r.name != name);
    ctx.check(Check::NoNew).await?;
    Ok(success())
}

/// Rename the relationship `old` of `target` to `new` wherever `exp` (evaluated on `table`)
/// traverses it.
fn rename_in_exp(
    source: &Source,
    catalog: Option<&SourceCatalog>,
    table: &QualifiedTable,
    exp: &mut Value,
    target: &QualifiedTable,
    old: &str,
    new: &str,
) {
    let Value::Object(obj) = exp else {
        return;
    };
    let entry = source.tables.iter().find(|t| t.table == *table);
    let entries: Vec<(String, Value)> = std::mem::take(obj).into_iter().collect();
    for (key, mut value) in entries {
        let mut key = key;
        match key.as_str() {
            "_and" | "$and" | "_or" | "$or" => match &mut value {
                Value::Array(items) => {
                    for i in items {
                        rename_in_exp(source, catalog, table, i, target, old, new);
                    }
                }
                other => rename_in_exp(source, catalog, table, other, target, old, new),
            },
            "_not" | "$not" => rename_in_exp(source, catalog, table, &mut value, target, old, new),
            "_exists" => {
                if let Some(t) = value.get("_table").and_then(|t| parse_table(t, "$").ok())
                    && let Some(w) = value.get_mut("_where")
                {
                    rename_in_exp(source, catalog, &t, w, target, old, new);
                }
            }
            name => {
                // Relationships are looked up under their old name: the metadata is renamed
                // after the permissions.
                if let Some(next) = entry.and_then(|e| relationship_target(e, name, catalog)) {
                    rename_in_exp(source, catalog, &next, &mut value, target, old, new);
                    if table == target && name == old {
                        key = new.to_owned();
                    }
                }
            }
        }
        obj.insert(key, value);
    }
}

async fn rename_relationship<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let a = Args::new(args)?;
    let source = a.source()?;
    let table = a.table("table")?;
    let name = a.non_empty("name")?;
    let new_name = a.non_empty("new_name")?;
    let cat = catalog(ctx, &source);
    let entry = table_ref(&ctx.metadata, &source, &table)?;
    if !relationship_exists(entry, &name) {
        return Err(relationship_not_found(&name));
    }
    if field_names(entry, cat.as_deref()).contains(&new_name) {
        return Err(already_exists(format!(
            "cannot rename relationship \"{name}\" to \"{new_name}\" in table \"{}\" as a column/relationship with the name already exists",
            table.v2_text()
        )));
    }
    let snapshot = source_ref(&ctx.metadata, &source)?.clone();
    let src = ctx
        .metadata
        .source_mut(&source)
        .expect("source checked above");
    for t in &mut src.tables {
        let here = t.table.clone();
        let mut exps: Vec<&mut Value> = vec![];
        exps.extend(
            t.insert_permissions
                .iter_mut()
                .map(|p| &mut p.permission.check),
        );
        exps.extend(
            t.select_permissions
                .iter_mut()
                .map(|p| &mut p.permission.filter),
        );
        for p in &mut t.update_permissions {
            exps.push(&mut p.permission.filter);
            exps.extend(p.permission.check.as_mut());
        }
        exps.extend(
            t.delete_permissions
                .iter_mut()
                .map(|p| &mut p.permission.filter),
        );
        for e in exps {
            rename_in_exp(
                &snapshot,
                cat.as_deref(),
                &here,
                e,
                &table,
                &name,
                &new_name,
            );
        }
    }
    let entry = table_mut(&mut ctx.metadata, &source, &table)?;
    for r in &mut entry.object_relationships {
        if r.name == name {
            r.name = new_name.clone();
        }
    }
    for r in &mut entry.array_relationships {
        if r.name == name {
            r.name = new_name.clone();
        }
    }
    ctx.check(Check::NoNew).await?;
    Ok(success())
}

async fn set_relationship_comment<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let a = Args::new(args)?;
    let source = a.source()?;
    let table = a.table("table")?;
    let name = a.non_empty("relationship")?;
    let comment = a.opt_string("comment")?;
    let entry = table_mut(&mut ctx.metadata, &source, &table)?;
    if let Some(r) = entry
        .object_relationships
        .iter_mut()
        .find(|r| r.name == name)
    {
        r.comment = comment;
    } else if let Some(r) = entry
        .array_relationships
        .iter_mut()
        .find(|r| r.name == name)
    {
        r.comment = comment;
    } else {
        return Err(relationship_not_found(&name));
    }
    ctx.check(Check::NoNew).await?;
    Ok(success())
}

// ---------------------------------------------------------------------------------------
// Permissions

/// Keys a permission definition must have, per kind.
fn required_permission_keys(kind: &str) -> &'static [&'static str] {
    match kind {
        "select" => &["columns", "filter"],
        "update" => &["columns", "filter"],
        "insert" => &["check"],
        _ => &["filter"],
    }
}

fn parse_permission<P: DeserializeOwned>(a: &Args<'_>, kind: &str) -> ApiResult<P> {
    let Some(perm) = a.raw("permission") else {
        return Err(parse_failed(
            "$.args",
            format!("parsing {PERM_DEF} failed, key \"permission\" not found"),
        ));
    };
    let p = Args::at(perm, "$.args.permission")?;
    for key in required_permission_keys(kind) {
        if !p.has(key) {
            return Err(p.missing(key));
        }
    }
    serde_json::from_value(perm.clone())
        .map_err(|e| parse_failed("$.args.permission", crate::response::aeson_message(&e)))
}

fn push_permission<P>(
    list: &mut Vec<Permission<P>>,
    role: String,
    permission: P,
    comment: Option<String>,
) {
    list.push(Permission {
        role,
        permission,
        comment,
    });
}

fn has_role<P>(list: &[Permission<P>], role: &str) -> bool {
    list.iter().any(|p| p.role == role)
}

async fn create_permission<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
    kind: &str,
) -> ApiResult<Value> {
    let a = Args::new(args)?;
    let source = a.source()?;
    let table = a.table("table")?;
    let role = a.string("role").map_err(|e| prefixed(e, PERM_DEF))?;
    let role = a.check_non_empty("role", role)?;
    let comment = a.opt_string("comment")?;
    enum Parsed {
        Insert(InsertPermission),
        Select(SelectPermission),
        Update(UpdatePermission),
        Delete(DeletePermission),
    }
    let parsed = match kind {
        "insert" => Parsed::Insert(parse_permission(&a, kind)?),
        "select" => Parsed::Select(parse_permission(&a, kind)?),
        "update" => Parsed::Update(parse_permission(&a, kind)?),
        _ => Parsed::Delete(parse_permission(&a, kind)?),
    };
    let entry = table_mut(&mut ctx.metadata, &source, &table)?;
    let exists = role == hge_core::ADMIN_ROLE
        || match kind {
            "insert" => has_role(&entry.insert_permissions, &role),
            "select" => has_role(&entry.select_permissions, &role),
            "update" => has_role(&entry.update_permissions, &role),
            _ => has_role(&entry.delete_permissions, &role),
        };
    if exists {
        return Err(already_exists(format!(
            "{kind} permission already defined on table \"{}\" with role \"{role}\"",
            table.v2_text()
        )));
    }
    match parsed {
        Parsed::Insert(p) => {
            push_permission(&mut entry.insert_permissions, role.clone(), p, comment)
        }
        Parsed::Select(p) => {
            push_permission(&mut entry.select_permissions, role.clone(), p, comment)
        }
        Parsed::Update(p) => {
            push_permission(&mut entry.update_permissions, role.clone(), p, comment)
        }
        Parsed::Delete(p) => {
            push_permission(&mut entry.delete_permissions, role.clone(), p, comment)
        }
    }
    let object_kind = format!("{kind}_permission");
    let name = table_item_name(&object_kind, &role, &source, &table);
    ctx.check(Check::object(&object_kind, name)).await?;
    Ok(success())
}

fn remove_role<P>(list: &mut Vec<Permission<P>>, role: &str) -> bool {
    let before = list.len();
    list.retain(|p| p.role != role);
    list.len() != before
}

async fn drop_permission<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
    kind: &str,
) -> ApiResult<Value> {
    let a = Args::new(args)?;
    let source = a.source()?;
    let table = a.table("table")?;
    let role = a.non_empty("role")?;
    let entry = table_mut(&mut ctx.metadata, &source, &table)?;
    let removed = match kind {
        "insert" => remove_role(&mut entry.insert_permissions, &role),
        "select" => remove_role(&mut entry.select_permissions, &role),
        "update" => remove_role(&mut entry.update_permissions, &role),
        _ => remove_role(&mut entry.delete_permissions, &role),
    };
    if !removed {
        return Err(ApiError::bad(
            Code::PermissionDenied,
            format!(
                "{kind} permission on \"{}\" for role \"{role}\" does not exist",
                table.v2_text()
            ),
        ));
    }
    ctx.check(Check::NoNew).await?;
    Ok(success())
}

fn set_comment<P>(list: &mut [Permission<P>], role: &str, comment: Option<String>) -> bool {
    match list.iter_mut().find(|p| p.role == role) {
        Some(p) => {
            p.comment = comment;
            true
        }
        None => false,
    }
}

async fn set_permission_comment<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let a = Args::new(args)?;
    let source = a.source()?;
    let table = a.table("table")?;
    let role = a.non_empty("role")?;
    let kind = a.string("permission")?;
    if !matches!(kind.as_str(), "insert" | "select" | "update" | "delete") {
        return Err(parse_failed(
            "$.args.permission",
            "perm_type should be one of 'insert', 'select', 'update', 'delete'",
        ));
    }
    let comment = a.opt_string("comment")?;
    let entry = table_mut(&mut ctx.metadata, &source, &table)?;
    let found = match kind.as_str() {
        "insert" => set_comment(&mut entry.insert_permissions, &role, comment),
        "select" => set_comment(&mut entry.select_permissions, &role, comment),
        "update" => set_comment(&mut entry.update_permissions, &role, comment),
        _ => set_comment(&mut entry.delete_permissions, &role, comment),
    };
    if !found {
        return Err(ApiError::bad(
            Code::PermissionDenied,
            format!(
                "'{kind}' permission on \"{}\" for role \"{role}\" does not exist",
                table.v2_text()
            ),
        ));
    }
    ctx.check(Check::NoNew).await?;
    Ok(success())
}

// ---------------------------------------------------------------------------------------
// Suggested relationships

/// Every foreign key of the source's user tables, as JSON rows.
const FOREIGN_KEYS_SQL: &str = "\
SELECT coalesce(json_agg(json_build_object( \
    'constraint', c.conname, \
    'from_schema', fn.nspname, 'from_name', f.relname, \
    'to_schema', tn.nspname, 'to_name', t.relname, \
    'from_columns', (SELECT json_agg(a.attname ORDER BY k.i) FROM unnest(c.conkey) WITH ORDINALITY k(n, i) \
                     JOIN pg_catalog.pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.n), \
    'to_columns', (SELECT json_agg(a.attname ORDER BY k.i) FROM unnest(c.confkey) WITH ORDINALITY k(n, i) \
                   JOIN pg_catalog.pg_attribute a ON a.attrelid = c.confrelid AND a.attnum = k.n)) \
  ORDER BY fn.nspname COLLATE \"C\", f.relname COLLATE \"C\", c.conname COLLATE \"C\"), '[]')::text \
FROM pg_catalog.pg_constraint c \
JOIN pg_catalog.pg_class f ON f.oid = c.conrelid \
JOIN pg_catalog.pg_namespace fn ON fn.oid = f.relnamespace \
JOIN pg_catalog.pg_class t ON t.oid = c.confrelid \
JOIN pg_catalog.pg_namespace tn ON tn.oid = t.relnamespace \
WHERE c.contype = 'f'";

fn table_json(schema: &str, name: &str) -> Value {
    json!({"schema": schema, "name": name})
}

/// Is there already a relationship on `table` to `target` joining `local` columns to
/// `remote` ones?
fn tracked_mapping(
    source: &Source,
    catalog: Option<&SourceCatalog>,
    table: &QualifiedTable,
    target: &QualifiedTable,
    local: &[String],
    remote: &[String],
) -> bool {
    let Some(entry) = source.tables.iter().find(|t| t.table == *table) else {
        return false;
    };
    let same = |a: &[String], b: &[String]| {
        let mut a = a.to_vec();
        let mut b = b.to_vec();
        a.sort();
        b.sort();
        a == b
    };
    let manual = |m: &hge_metadata::ManualRelConfig| {
        let keys: Vec<String> = m.column_mapping.keys().cloned().collect();
        let values: Vec<String> = m.column_mapping.values().cloned().collect();
        same(&keys, local) && same(&values, remote)
    };
    let matches_object = entry.object_relationships.iter().any(|r| {
        relationship_target(entry, &r.name, catalog).as_ref() == Some(target)
            && match &r.using {
                ObjectRelUsing::ForeignKey {
                    foreign_key_constraint_on: ObjectFkOn::Column(c),
                } => same(std::slice::from_ref(c), local),
                ObjectRelUsing::ForeignKey {
                    foreign_key_constraint_on: ObjectFkOn::Columns(cs),
                } => same(cs, local),
                ObjectRelUsing::ForeignKey {
                    foreign_key_constraint_on: ObjectFkOn::Remote(fk),
                } => same(&fk.columns(), remote),
                ObjectRelUsing::Manual {
                    manual_configuration: m,
                } => manual(m),
            }
    });
    matches_object
        || entry.array_relationships.iter().any(|r| {
            relationship_target(entry, &r.name, catalog).as_ref() == Some(target)
                && match &r.using {
                    ArrayRelUsing::ForeignKey {
                        foreign_key_constraint_on: fk,
                    } => same(&fk.columns(), remote),
                    ArrayRelUsing::Manual {
                        manual_configuration: m,
                    } => manual(m),
                }
        })
}

async fn suggest_relationships<R: Runtime>(ctx: &mut Ctx<'_, R>, args: &Value) -> ApiResult<Value> {
    let a = Args::new(args)?;
    let source = a.source()?;
    let omit_tracked = a.bool_or("omit_tracked", false)?;
    let tables: Option<Vec<QualifiedTable>> = match a.raw("tables") {
        None => None,
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .enumerate()
                .map(|(i, t)| parse_table(t, &format!("$.args.tables[{i}]")))
                .collect::<ApiResult<_>>()?,
        ),
        Some(_) => {
            return Err(parse_failed(
                "$.args.tables",
                "parsing [] failed, expected Array",
            ));
        }
    };
    let src = source_ref(&ctx.metadata, &source)?.clone();
    let cat = catalog(ctx, &source);
    let pg = ctx
        .rt
        .source(ctx.prepared.as_ref(), &source)
        .ok_or_else(|| super::sources_tables::source_not_found(&source))?;
    let rows: Vec<Value> = pg
        .query_text(FOREIGN_KEYS_SQL, &[])
        .await?
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let tracked: HashSet<&QualifiedTable> = src.tables.iter().map(|t| &t.table).collect();
    // Only foreign keys between tracked tables, touching one of `tables` if given.
    let selected = |from: &QualifiedTable, to: &QualifiedTable| {
        tracked.contains(from)
            && tracked.contains(to)
            && tables
                .as_ref()
                .is_none_or(|list| list.contains(from) || list.contains(to))
    };
    let text = |row: &Value, key: &str| row[key].as_str().unwrap_or_default().to_owned();
    let strings = |row: &Value, key: &str| -> Vec<String> {
        row[key]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut out = vec![];
    for row in &rows {
        let from = QualifiedTable::new(text(row, "from_schema"), text(row, "from_name"));
        let to = QualifiedTable::new(text(row, "to_schema"), text(row, "to_name"));
        let from_cols = strings(row, "from_columns");
        let to_cols = strings(row, "to_columns");
        let constraint = text(row, "constraint");
        if !selected(&from, &to) {
            continue;
        }
        if !(omit_tracked
            && tracked_mapping(&src, cat.as_deref(), &from, &to, &from_cols, &to_cols))
        {
            out.push(json!({
                "type": "object",
                "from": {"table": table_json(&from.schema, &from.name), "columns": from_cols, "constraint_name": constraint},
                "to": {"table": table_json(&to.schema, &to.name), "columns": to_cols},
            }));
        }
        if !(omit_tracked
            && tracked_mapping(&src, cat.as_deref(), &to, &from, &to_cols, &from_cols))
        {
            out.push(json!({
                "type": "array",
                "from": {"table": table_json(&to.schema, &to.name), "columns": to_cols},
                "to": {"table": table_json(&from.schema, &from.name), "columns": from_cols, "constraint_name": constraint},
            }));
        }
    }
    Ok(json!({ "relationships": out }))
}
