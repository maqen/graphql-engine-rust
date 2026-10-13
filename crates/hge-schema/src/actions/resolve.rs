//! Resolving `actions` and `custom_types` against the tracked tables (V2's
//! `resolveCustomTypes` / `resolveAction`). Invalid objects become inconsistencies.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use hge_core::{InconsistentObject, QualifiedTable};
use hge_ir::action::{ActionOutput, OutputBase, OutputType};
use hge_metadata::Metadata;
use hge_metadata::actions::{Action, ActionKind, ActionType, CustomTypes, RelType};
use indexmap::IndexMap;

use crate::build::{TableSchemaNames, table_schema_names};
use crate::model::TypeRef;
use crate::scalars::BUILTIN_SCALARS;
use crate::tables::Tables;

/// Postgres scalar type names usable in custom types (V2's `pgScalarTranslations` plus the
/// usual base types). Their values are passed through to and from the handler unchanged.
pub const PG_SCALARS: &[&str] = &[
    "serial",
    "bigserial",
    "smallint",
    "int2",
    "int",
    "integer",
    "int4",
    "bigint",
    "int8",
    "real",
    "float4",
    "double precision",
    "float8",
    "numeric",
    "decimal",
    "money",
    "boolean",
    "bool",
    "bpchar",
    "char",
    "character",
    "varchar",
    "character varying",
    "text",
    "citext",
    "date",
    "timestamp",
    "timestamptz",
    "time",
    "timetz",
    "interval",
    "json",
    "jsonb",
    "geometry",
    "geography",
    "raster",
    "uuid",
    "ltree",
    "lquery",
    "ltxtquery",
    "inet",
    "cidr",
    "macaddr",
    "bytea",
    "oid",
    "tsvector",
    "tsquery",
    "xml",
    "bit",
    "varbit",
];

pub fn is_pg_scalar(name: &str) -> bool {
    PG_SCALARS.contains(&name)
}

fn is_builtin(name: &str) -> bool {
    BUILTIN_SCALARS.contains(&name)
}

/// A field of a custom input object or object, or an action argument.
#[derive(Debug, Clone)]
pub struct CustomField {
    pub name: String,
    pub ty: TypeRef,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EnumTypeInfo {
    pub name: String,
    pub description: Option<String>,
    pub values: Vec<(String, Option<String>)>,
}

#[derive(Debug, Clone)]
pub struct InputObjectTypeInfo {
    pub name: String,
    pub description: Option<String>,
    pub fields: Vec<CustomField>,
}

#[derive(Debug, Clone)]
pub struct ObjectTypeInfo {
    pub name: String,
    pub description: Option<String>,
    pub fields: Vec<CustomField>,
    pub relationships: Vec<Arc<ActionRelInfo>>,
}

/// A relationship from a custom object type to a table.
#[derive(Debug, Clone)]
pub struct ActionRelInfo {
    pub name: String,
    pub array: bool,
    pub source: Arc<str>,
    pub table: QualifiedTable,
    /// `(object field name, table column)`.
    pub mapping: Vec<(String, String)>,
}

/// Validated custom types.
#[derive(Debug, Clone, Default)]
pub struct CustomTypeDefs {
    pub scalars: IndexMap<String, Option<String>>,
    pub enums: IndexMap<String, EnumTypeInfo>,
    pub input_objects: IndexMap<String, InputObjectTypeInfo>,
    pub objects: IndexMap<String, Arc<ObjectTypeInfo>>,
}

impl CustomTypeDefs {
    /// A named type usable as an input: Postgres scalar, custom or built-in scalar, enum or
    /// input object.
    fn is_input(&self, name: &str) -> bool {
        is_pg_scalar(name)
            || is_builtin(name)
            || self.scalars.contains_key(name)
            || self.enums.contains_key(name)
            || self.input_objects.contains_key(name)
    }

    fn is_scalar_or_enum(&self, name: &str) -> bool {
        is_pg_scalar(name)
            || is_builtin(name)
            || self.scalars.contains_key(name)
            || self.enums.contains_key(name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionRoot {
    Query,
    SyncMutation,
    AsyncMutation,
}

/// A resolved action.
#[derive(Debug, Clone)]
pub struct ActionInfo {
    pub name: String,
    pub description: Option<String>,
    pub root: ActionRoot,
    pub arguments: Vec<CustomField>,
    pub output_type: TypeRef,
    /// The custom object of the output, if it is an object.
    pub output_object: Option<Arc<ObjectTypeInfo>>,
    pub output: Arc<ActionOutput>,
    /// Roles with access (explicit permissions plus inherited roles); admin always has it.
    pub roles: HashSet<String>,
}

impl ActionInfo {
    pub fn allowed(&self, role: &str) -> bool {
        role == hge_core::ADMIN_ROLE || self.roles.contains(role)
    }
}

/// All actions and custom types of one metadata version.
#[derive(Debug, Clone, Default)]
pub struct ResolvedActions {
    pub types: CustomTypeDefs,
    pub actions: IndexMap<String, Arc<ActionInfo>>,
}

/// Resolve actions and custom types. Returns what resolved plus inconsistency messages.
pub fn resolve_actions(
    metadata: &Metadata,
    tables: &Tables,
) -> (ResolvedActions, Vec<InconsistentObject>) {
    let mut inconsistencies = vec![];
    let action_name = |a: &serde_json::Value| -> String {
        a.get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("?")
            .to_owned()
    };
    if metadata.actions.is_empty() && metadata.custom_types.is_none() {
        return (ResolvedActions::default(), inconsistencies);
    }
    let parsed = CustomTypes::from_json(metadata.custom_types.as_ref());
    let types = match parsed
        .as_ref()
        .map_err(|e| vec![e.clone()])
        .and_then(|ct| resolve_custom_types(ct, tables))
    {
        Ok(t) => t,
        Err(reasons) => {
            let message = match reasons.as_slice() {
                [one] => format!("validation for the given custom types failed because {one}"),
                many => {
                    let mut m = String::from(
                        "validation for the given custom types failed for the following reasons:\n",
                    );
                    for r in many {
                        m.push_str(&format!("  • {r}\n"));
                    }
                    m
                }
            };
            // V2 shows its parsed custom types (unparsable ones never get this far there).
            let definition = match &parsed {
                Ok(ct) => ct.to_v2_json(),
                Err(_) => metadata.custom_types.clone().unwrap_or_default(),
            };
            inconsistencies.push(InconsistentObject::custom_types(message, definition));
            for a in &metadata.actions {
                let name = action_name(a);
                // V2 marks every action, without its usual `in action "…"; ` prefix.
                inconsistencies.push(InconsistentObject::action(
                    &name,
                    "custom types are inconsistent",
                    create_action_json(a),
                ));
            }
            return (ResolvedActions::default(), inconsistencies);
        }
    };

    // V2 rejects the whole metadata (`parseListAsMap`); we drop every action of that name.
    let duplicated = duplicates(
        metadata
            .actions
            .iter()
            .filter_map(|a| a.get("name").and_then(|n| n.as_str())),
    );
    if !duplicated.is_empty() {
        let reason = format!(
            "multiple declarations exist for the following actions: {}",
            duplicated.join(", ")
        );
        for a in metadata
            .actions
            .iter()
            .filter(|a| duplicated.contains(&action_name(a)))
        {
            inconsistencies.push(InconsistentObject::action(
                &action_name(a),
                reason.clone(),
                create_action_json(a),
            ));
        }
    }
    let table_names = table_schema_names(tables);
    let inherited = inherited_roles(metadata);
    let mut actions = IndexMap::new();
    for raw in &metadata.actions {
        let action = match Action::from_json(raw) {
            Ok(a) => a,
            Err(e) => {
                let name = e.name.clone().unwrap_or_else(|| action_name(raw));
                inconsistencies.push(InconsistentObject::action(
                    &name,
                    format!("in action \"{name}\"; {}", e.message),
                    raw.clone(),
                ));
                continue;
            }
        };
        if duplicated.contains(&action.name) {
            continue;
        }
        match resolve_action(&action, &types, &inherited)
            .and_then(|info| check_conflicts(&info, &types, &table_names).map(|()| info))
        {
            Ok(info) => {
                actions.insert(info.name.clone(), Arc::new(info));
            }
            Err(e) => inconsistencies.push(InconsistentObject::action(
                &action.name,
                format!("in action \"{}\"; {e}", action.name),
                action.to_create_action_json(),
            )),
        }
    }
    (ResolvedActions { types, actions }, inconsistencies)
}

/// The `definition` V2 shows for an inconsistent action (`ToJSON CreateAction`); an
/// action that does not parse is shown as written.
fn create_action_json(raw: &serde_json::Value) -> serde_json::Value {
    Action::from_json(raw).map_or_else(|_| raw.clone(), |a| a.to_create_action_json())
}

/// Inherited role → its (transitive) parent roles.
fn inherited_roles(metadata: &Metadata) -> HashMap<String, BTreeSet<String>> {
    let direct: HashMap<&str, &Vec<String>> = metadata
        .inherited_roles
        .iter()
        .map(|r| (r.role_name.as_str(), &r.role_set))
        .collect();
    let mut out = HashMap::new();
    for name in direct.keys() {
        let mut all = BTreeSet::new();
        let mut stack: Vec<&str> = vec![name];
        while let Some(r) = stack.pop() {
            if let Some(parents) = direct.get(r) {
                for p in parents.iter() {
                    if all.insert(p.clone()) {
                        stack.push(p);
                    }
                }
            }
        }
        out.insert((*name).to_owned(), all);
    }
    out
}

fn parse_type(s: &str) -> Option<TypeRef> {
    async_graphql_parser::types::Type::new(s).map(|t| TypeRef::from_ast(&t))
}

fn dquote_list<'a>(items: impl IntoIterator<Item = &'a String>) -> String {
    items
        .into_iter()
        .map(|i| format!("\"{i}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

fn duplicates<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut dups = BTreeSet::new();
    for n in names {
        if !seen.insert(n) {
            dups.insert(n.to_owned());
        }
    }
    dups.into_iter().collect()
}

fn resolve_custom_types(ct: &CustomTypes, tables: &Tables) -> Result<CustomTypeDefs, Vec<String>> {
    let mut errors = vec![];
    let all_names = ct
        .scalars
        .iter()
        .map(|t| t.name.as_str())
        .chain(ct.enums.iter().map(|t| t.name.as_str()))
        .chain(ct.input_objects.iter().map(|t| t.name.as_str()))
        .chain(ct.objects.iter().map(|t| t.name.as_str()));
    let dups = duplicates(all_names);
    if !dups.is_empty() {
        errors.push(format!("duplicate type names: {}", dquote_list(&dups)));
    }

    let mut defs = CustomTypeDefs::default();
    for s in &ct.scalars {
        defs.scalars.insert(s.name.clone(), s.description.clone());
    }
    for e in &ct.enums {
        let dups = duplicates(e.values.iter().map(|v| v.value.as_str()));
        if !dups.is_empty() {
            errors.push(format!(
                "the enum type \"{}\" has duplicate values: {}",
                e.name,
                dquote_list(&dups)
            ));
        }
        defs.enums.insert(
            e.name.clone(),
            EnumTypeInfo {
                name: e.name.clone(),
                description: e.description.clone(),
                values: e
                    .values
                    .iter()
                    .map(|v| (v.value.clone(), v.description.clone()))
                    .collect(),
            },
        );
    }
    // Input objects (field types checked once all names are known).
    for io in &ct.input_objects {
        let mut fields = vec![];
        for f in &io.fields {
            match parse_type(&f.ty) {
                Some(ty) => fields.push(CustomField {
                    name: f.name.clone(),
                    ty,
                    description: f.description.clone(),
                }),
                None => errors.push(format!("not a valid GraphQL type: {}", f.ty)),
            }
        }
        let dups = duplicates(io.fields.iter().map(|f| f.name.as_str()));
        if !dups.is_empty() {
            errors.push(format!(
                "the input object \"{}\" has duplicate fields: {}",
                io.name,
                dquote_list(&dups)
            ));
        }
        defs.input_objects.insert(
            io.name.clone(),
            InputObjectTypeInfo {
                name: io.name.clone(),
                description: io.description.clone(),
                fields,
            },
        );
    }
    for io in defs.input_objects.values() {
        for f in &io.fields {
            if !defs.is_input(f.ty.base()) {
                errors.push(format!(
                    "the type \"{}\" for field \"{}\" in  input object type \"{}\" does not exist",
                    f.ty.base(),
                    f.name,
                    io.name
                ));
            }
        }
    }

    let object_names: HashSet<&str> = ct.objects.iter().map(|o| o.name.as_str()).collect();
    for obj in &ct.objects {
        let mut fields = vec![];
        for f in &obj.fields {
            if f.arguments.as_ref().is_some_and(|a| !a.is_null()) {
                errors.push(format!("the object \"{}\" can't have arguments", obj.name));
            }
            let Some(ty) = parse_type(&f.ty) else {
                errors.push(format!("not a valid GraphQL type: {}", f.ty));
                continue;
            };
            let base = ty.base();
            if !defs.is_scalar_or_enum(base) && !object_names.contains(base) {
                errors.push(format!(
                    "the type \"{base}\" for field \"{}\" in  object type \"{}\" does not exist",
                    f.name, obj.name
                ));
            }
            fields.push(CustomField {
                name: f.name.clone(),
                ty,
                description: f.description.clone(),
            });
        }
        let dups = duplicates(
            obj.fields
                .iter()
                .map(|f| f.name.as_str())
                .chain(obj.relationships.iter().map(|r| r.name.as_str())),
        );
        if !dups.is_empty() {
            errors.push(format!(
                "the object \"{}\" has duplicate fields: {}",
                obj.name,
                dquote_list(&dups)
            ));
        }
        let sources: BTreeSet<&str> = obj
            .relationships
            .iter()
            .map(|r| r.source.as_str())
            .collect();
        if sources.len() > 1 {
            errors.push(format!(
                "the object \"{}\" has relationships refers to tables in multiple sources",
                obj.name
            ));
        }
        let mut relationships = vec![];
        for rel in &obj.relationships {
            let Some(table) = tables.get(&rel.source, &rel.remote_table) else {
                errors.push(format!(
                    "the remote table \"{}\" for relationship \"{}\" of object type \"{}\" does not exist",
                    rel.remote_table.v2_text(), rel.name, obj.name
                ));
                continue;
            };
            let mut mapping = vec![];
            for (field, column) in &rel.field_mapping {
                match fields.iter().find(|f| &f.name == field) {
                    None => errors.push(format!(
                        "the field \"{field}\" for relationship \"{}\" in object type \"{}\" does not exist",
                        rel.name, obj.name
                    )),
                    Some(f) if matches!(f.ty.nullable(), TypeRef::List(_)) => errors.push(format!(
                        "the type of the field \"{field}\" for relationship \"{}\" in object type \"{}\" is a list type",
                        rel.name, obj.name
                    )),
                    Some(_) => {}
                }
                if !table.columns.contains_key(column) {
                    errors.push(format!(
                        "the column \"{column}\" of remote table \"{}\" for relationship \"{}\" of object type \"{}\" does not exist",
                        rel.remote_table.v2_text(), rel.name, obj.name
                    ));
                }
                mapping.push((field.clone(), column.clone()));
            }
            relationships.push(Arc::new(ActionRelInfo {
                name: rel.name.clone(),
                array: rel.rel_type == RelType::Array,
                source: Arc::from(rel.source.as_str()),
                table: rel.remote_table.clone(),
                mapping,
            }));
        }
        defs.objects.insert(
            obj.name.clone(),
            Arc::new(ObjectTypeInfo {
                name: obj.name.clone(),
                description: obj.description.clone(),
                fields,
                relationships,
            }),
        );
    }

    if errors.is_empty() {
        Ok(defs)
    } else {
        Err(errors)
    }
}

impl CustomTypeDefs {
    /// Whether `name` is a custom type, and if so whether it is a scalar.
    fn custom_kind(&self, name: &str) -> Option<bool> {
        if self.scalars.contains_key(name) {
            Some(true)
        } else if self.enums.contains_key(name)
            || self.input_objects.contains_key(name)
            || self.objects.contains_key(name)
        {
            Some(false)
        } else {
            None
        }
    }

    /// Custom types reachable from the given type names (through input object and object
    /// fields), in first-seen order.
    fn reachable<'a>(&'a self, roots: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
        let mut seen = HashSet::new();
        let mut out = vec![];
        let mut stack: Vec<&str> = roots.into_iter().collect();
        stack.reverse();
        while let Some(name) = stack.pop() {
            if self.custom_kind(name).is_none() || !seen.insert(name) {
                continue;
            }
            out.push(name);
            let fields = match (self.input_objects.get(name), self.objects.get(name)) {
                (Some(io), _) => &io.fields,
                (_, Some(o)) => &o.fields,
                _ => continue,
            };
            stack.extend(fields.iter().rev().map(|f| f.ty.base()));
        }
        out
    }
}

/// Name conflicts of an action with what the tracked tables put in the schema: its root
/// fields, the types it brings in (custom types and the async result object), V2 fails the
/// whole schema build on these (§3 and §10 of the schema spec); we drop the action.
fn check_conflicts(
    action: &ActionInfo,
    types: &CustomTypeDefs,
    tables: &TableSchemaNames,
) -> Result<(), String> {
    let roots: &[(&str, &HashSet<String>)] = match action.root {
        ActionRoot::Query => &[(crate::build::QUERY_ROOT, &tables.query_fields)],
        ActionRoot::SyncMutation => &[(crate::build::MUTATION_ROOT, &tables.mutation_fields)],
        ActionRoot::AsyncMutation => &[
            (crate::build::MUTATION_ROOT, &tables.mutation_fields),
            (crate::build::QUERY_ROOT, &tables.query_fields),
        ],
    };
    for (root, fields) in roots {
        if fields.contains(&action.name) {
            return Err(format!(
                "Encountered conflicting definitions in the selection set for '{root}' for field '{}'. Fields must not be defined more than once across all sources.",
                action.name
            ));
        }
    }
    if action.root == ActionRoot::AsyncMutation
        && action.output_object.is_some()
        && (tables.types.contains_key(&action.name) || types.custom_kind(&action.name).is_some())
    {
        return Err(format!(
            "the result type \"{}\" of the asynchronous action conflicts with an existing GraphQL type of the same name",
            action.name
        ));
    }
    let used = types.reachable(
        action
            .arguments
            .iter()
            .map(|a| a.ty.base())
            .chain([action.output_type.base()]),
    );
    for name in used {
        let custom_scalar = types.custom_kind(name) == Some(true);
        // A custom scalar may share its name with a scalar the tables use.
        if let Some(&table_scalar) = tables.types.get(name)
            && !(custom_scalar && table_scalar)
        {
            return Err(format!(
                "the custom type \"{name}\" conflicts with an existing GraphQL type of the same name"
            ));
        }
    }
    Ok(())
}

fn output_type(ty: &TypeRef) -> OutputType {
    match ty {
        TypeRef::Named(n) => OutputType::Named(n.clone()),
        TypeRef::List(t) => OutputType::List(Box::new(output_type(t))),
        TypeRef::NonNull(t) => OutputType::NonNull(Box::new(output_type(t))),
    }
}

fn resolve_action(
    action: &Action,
    types: &CustomTypeDefs,
    inherited: &HashMap<String, BTreeSet<String>>,
) -> Result<ActionInfo, String> {
    let def = &action.definition;
    let mut arguments = vec![];
    for arg in &def.arguments {
        let ty =
            parse_type(&arg.ty).ok_or_else(|| format!("not a valid GraphQL type: {}", arg.ty))?;
        if !types.is_input(ty.base()) {
            return Err(format!(
                "the type: \"{}\" is not defined in custom types or it is not a scalar/enum/input_object",
                ty.base()
            ));
        }
        arguments.push(CustomField {
            name: arg.name.clone(),
            ty,
            description: arg.description.clone(),
        });
    }
    let out_ty = parse_type(&def.output_type)
        .ok_or_else(|| format!("not a valid GraphQL type: {}", def.output_type))?;
    let base = out_ty.base();
    let (output_object, output_base) = if is_pg_scalar(base) {
        (None, OutputBase::CustomScalar)
    } else if let Some(obj) = types.objects.get(base) {
        let required = obj
            .fields
            .iter()
            .filter(|f| f.ty.is_non_null())
            .map(|f| f.name.clone())
            .collect();
        (Some(obj.clone()), OutputBase::Object { required })
    } else if is_builtin(base) {
        (None, OutputBase::BuiltinScalar)
    } else if types.scalars.contains_key(base) {
        (None, OutputBase::CustomScalar)
    } else {
        return Err(format!(
            "the type: \"{base}\" is not an object or scalar type defined in custom types"
        ));
    };

    let root = match def.action_type {
        ActionType::Query => ActionRoot::Query,
        ActionType::Mutation(ActionKind::Synchronous) => ActionRoot::SyncMutation,
        ActionType::Mutation(ActionKind::Asynchronous) => ActionRoot::AsyncMutation,
    };
    if let Some(obj) = &output_object
        && !obj.relationships.is_empty()
    {
        let object_fields: Vec<String> = obj
            .fields
            .iter()
            .filter(|f| types.objects.contains_key(f.ty.base()))
            .map(|f| f.name.clone())
            .collect();
        if root == ActionRoot::AsyncMutation {
            if !object_fields.is_empty() {
                return Err(format!(
                    "Async action relations cannot be used with object fields: {}",
                    dquote_list(&object_fields)
                ));
            }
        } else {
            let bad: Vec<String> = obj
                .relationships
                .iter()
                .filter(|r| r.mapping.iter().any(|(f, _)| object_fields.contains(f)))
                .map(|r| r.name.clone())
                .collect();
            if !bad.is_empty() {
                return Err(format!(
                    "Relationships cannot be defined with nested object fields: {}",
                    dquote_list(&bad)
                ));
            }
        }
    }

    let explicit: HashSet<String> = action.permissions.iter().map(|p| p.role.clone()).collect();
    let mut roles = explicit.clone();
    for (role, parents) in inherited {
        if parents.iter().any(|p| explicit.contains(p)) {
            roles.insert(role.clone());
        }
    }

    Ok(ActionInfo {
        name: action.name.clone(),
        description: action.comment.clone(),
        root,
        arguments,
        output: Arc::new(ActionOutput {
            ty: output_type(&out_ty),
            base: output_base,
        }),
        output_type: out_ty,
        output_object,
        roles,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `resolve_actions` with the inconsistency reasons only.
    fn resolve(m: &Metadata, tables: &Tables) -> (ResolvedActions, Vec<String>) {
        let (ra, errs) = resolve_actions(m, tables);
        (ra, errs.into_iter().map(|o| o.reason).collect())
    }

    fn md(actions: serde_json::Value, custom_types: serde_json::Value) -> Metadata {
        hge_metadata::from_json(json!({
            "version": 3, "sources": [], "actions": actions, "custom_types": custom_types,
            "inherited_roles": [{"role_name": "both", "role_set": ["user", "other"]}]
        }))
        .unwrap()
    }

    #[test]
    fn resolves_action_and_inherited_roles() {
        let m = md(
            json!([{
                "name": "login",
                "definition": {"type": "mutation", "output_type": "LoginOutput!",
                    "handler": "h", "arguments": [{"name": "u", "type": "String!"}]},
                "permissions": [{"role": "user"}]
            }]),
            json!({"objects": [{"name": "LoginOutput",
                "fields": [{"name": "token", "type": "String!"}, {"name": "id", "type": "uuid"}]}]}),
        );
        let (ra, errs) = resolve(&m, &Tables::default());
        assert!(errs.is_empty(), "{errs:?}");
        let a = &ra.actions["login"];
        assert_eq!(a.root, ActionRoot::SyncMutation);
        assert!(a.allowed("user") && a.allowed("both") && a.allowed("admin"));
        assert!(!a.allowed("other"));
        assert_eq!(
            a.output.base,
            OutputBase::Object {
                required: vec!["token".into()]
            }
        );
    }

    #[test]
    fn reports_custom_type_errors() {
        let m = md(
            json!([{"name": "a", "definition": {"output_type": "O", "handler": "h"}}]),
            json!({
                "objects": [{"name": "O", "fields": [{"name": "f", "type": "Nope"}]}],
                "scalars": [{"name": "O"}]
            }),
        );
        let (ra, errs) = resolve(&m, &Tables::default());
        assert!(ra.actions.is_empty());
        assert_eq!(
            errs[0],
            "validation for the given custom types failed for the following reasons:\n  \
             • duplicate type names: \"O\"\n  \
             • the type \"Nope\" for field \"f\" in  object type \"O\" does not exist\n"
        );
        assert_eq!(errs[1], "custom types are inconsistent");
    }

    #[test]
    fn rejects_bad_argument_and_output_types() {
        let m = md(
            json!([
                {"name": "a", "definition": {"output_type": "O", "handler": "h",
                    "arguments": [{"name": "x", "type": "O"}]}},
                {"name": "b", "definition": {"output_type": "E", "handler": "h"}}
            ]),
            json!({
                "objects": [{"name": "O", "fields": [{"name": "f", "type": "Int"}]}],
                "enums": [{"name": "E", "values": [{"value": "A"}]}]
            }),
        );
        let (_, errs) = resolve(&m, &Tables::default());
        assert_eq!(
            errs,
            vec![
                "in action \"a\"; the type: \"O\" is not defined in custom types or it is not a scalar/enum/input_object",
                "in action \"b\"; the type: \"E\" is not an object or scalar type defined in custom types",
            ]
        );
    }

    /// Tables with one tracked table `author(id int4, name text)`.
    fn author_tables() -> Tables {
        use hge_core::catalog::{ColumnInfo, SourceCatalog, TableCatalog, TableKind};
        let m = hge_metadata::from_json(json!({
            "version": 3,
            "sources": [{"name": "default", "kind": "postgres",
                "configuration": {"connection_info": {"database_url": "postgres://x"}},
                "tables": [{"table": {"schema": "public", "name": "author"}}]}]
        }))
        .unwrap();
        let column = |name: &str, ty: &str| ColumnInfo {
            name: name.to_owned(),
            ty: hge_core::ColumnType::builtin(ty),
            nullable: false,
            has_default: false,
            is_identity: false,
            is_identity_always: false,
            is_generated: false,
            description: None,
        };
        let name = QualifiedTable::public("author");
        let table = TableCatalog {
            name: name.clone(),
            kind: TableKind::Table,
            description: None,
            columns: [column("id", "int4"), column("name", "text")]
                .into_iter()
                .map(|c| (c.name.clone(), c))
                .collect(),
            primary_key: Some(vec!["id".to_owned()]),
            unique_constraints: vec![],
            foreign_keys: vec![],
            constraints: vec![],
            mutability: hge_core::TableMutability::ALL,
        };
        let catalog = SourceCatalog {
            tables: [(name, table)].into_iter().collect(),
            enums: HashMap::new(),
            functions: HashMap::new(),
        };
        let (tables, errs) =
            crate::resolve_tables(&m, &[("default".to_owned(), catalog)].into_iter().collect());
        assert!(errs.is_empty(), "{errs:?}");
        tables
    }

    #[test]
    fn reports_name_conflicts() {
        let m = md(
            json!([
                {"name": "author_by_pk", "definition": {"type": "query", "output_type": "String", "handler": "h"}},
                {"name": "uses_bool_exp", "definition": {"type": "query", "output_type": "String", "handler": "h",
                    "arguments": [{"name": "w", "type": "author_bool_exp"}]}},
                {"name": "author", "definition": {"kind": "asynchronous", "output_type": "O", "handler": "h"}},
                {"name": "author_order_by", "definition": {"kind": "asynchronous", "output_type": "O", "handler": "h"}},
                {"name": "String_out", "definition": {"type": "query", "output_type": "O", "handler": "h"}},
                {"name": "dup", "definition": {"output_type": "String", "handler": "h"}},
                {"name": "dup", "definition": {"type": "query", "output_type": "String", "handler": "h"}},
                {"name": "ok", "definition": {"type": "query", "output_type": "text", "handler": "h",
                    "arguments": [{"name": "s", "type": "Int"}]}}
            ]),
            json!({
                "objects": [{"name": "O", "fields": [{"name": "f", "type": "String"}]}],
                "input_objects": [{"name": "author_bool_exp", "fields": [{"name": "f", "type": "Int"}]}],
                "scalars": [{"name": "Int"}]
            }),
        );
        let (ra, errs) = resolve(&m, &author_tables());
        assert_eq!(
            errs,
            vec![
                // One inconsistent object per declaration.
                "multiple declarations exist for the following actions: dup",
                "multiple declarations exist for the following actions: dup",
                "in action \"author_by_pk\"; Encountered conflicting definitions in the selection set for 'query_root' for field 'author_by_pk'. Fields must not be defined more than once across all sources.",
                "in action \"uses_bool_exp\"; the custom type \"author_bool_exp\" conflicts with an existing GraphQL type of the same name",
                "in action \"author\"; Encountered conflicting definitions in the selection set for 'query_root' for field 'author'. Fields must not be defined more than once across all sources.",
                "in action \"author_order_by\"; the result type \"author_order_by\" of the asynchronous action conflicts with an existing GraphQL type of the same name",
            ]
        );
        assert_eq!(
            ra.actions.keys().collect::<Vec<_>>(),
            vec!["String_out", "ok"]
        );
    }
}
