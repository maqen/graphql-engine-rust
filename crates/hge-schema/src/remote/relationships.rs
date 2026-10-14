//! Remote relationships from tables to remote schemas (spec 08 §8): the metadata
//! definition, its validation against the remote's original types, and the field it adds
//! to the table's type. Lowering to a join request is in `lower.rs`.

use std::collections::HashMap;
use std::sync::Arc;

use hge_core::{ColumnType, InconsistentObject};
use indexmap::IndexMap;
use serde_json::{Map, Value as Json};

use super::{RemoteField, RemoteSchemaInfo, RemoteSchemas};
use crate::functions::{ComputedFieldInfo, ComputedReturn};
use crate::model::*;
use crate::tables::{TableInfo, Tables};

/// One level of `remote_field`: a field of the remote and its argument template.
#[derive(Debug, Clone, PartialEq)]
pub struct Hop {
    /// The field's original name.
    pub field: String,
    /// Argument name → template (`"$x"` is the join field `x`).
    pub arguments: Map<String, Json>,
}

/// A relationship to a remote schema, as written in metadata (legacy or
/// `to_remote_schema`).
#[derive(Debug, Clone, PartialEq)]
pub struct ToRemoteSchema {
    pub remote_schema: String,
    pub lhs_fields: Vec<String>,
    pub hops: Vec<Hop>,
}

/// A parsed remote relationship definition.
#[derive(Debug, Clone, PartialEq)]
pub enum Definition {
    ToRemoteSchema(ToRemoteSchema),
    ToSource,
}

/// Which formats a definition may use (spec 08 §8.1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Format {
    /// Metadata entries: any format.
    Lenient,
    /// Commands with a `definition`: `to_remote_schema` or `to_source`.
    Strict,
}

/// Parse a relationship's `definition`. Errors are V2's messages.
pub fn parse_definition(def: &Json, format: Format) -> Result<Definition, String> {
    let keys: &[&str] = match format {
        Format::Lenient => &["remote_schema", "to_source", "to_remote_schema"],
        Format::Strict => &["to_source", "to_remote_schema"],
    };
    let Json::Object(obj) = def else {
        return Err(format!(
            "expected Object, but encountered {}",
            super::introspection::kind_of(def)
        ));
    };
    let present: Vec<&str> = keys
        .iter()
        .copied()
        .filter(|k| obj.contains_key(*k))
        .collect();
    if present.len() != 1 {
        let name = match format {
            Format::Lenient => "lenient",
            Format::Strict => "strict",
        };
        return Err(format!(
            "remote relationship definition ({name} format) expects exactly one of: {}",
            keys.join(", ")
        ));
    }
    match present[0] {
        "remote_schema" => {
            parse_to_remote_schema(obj, "hasura_fields").map(Definition::ToRemoteSchema)
        }
        "to_remote_schema" => match &obj["to_remote_schema"] {
            Json::Object(o) => {
                parse_to_remote_schema(o, "lhs_fields").map(Definition::ToRemoteSchema)
            }
            other => Err(format!(
                "expected Object, but encountered {}",
                super::introspection::kind_of(other)
            )),
        },
        _ => Ok(Definition::ToSource),
    }
}

/// `{remote_schema, <fields_key>, remote_field}`.
fn parse_to_remote_schema(
    obj: &Map<String, Json>,
    fields_key: &str,
) -> Result<ToRemoteSchema, String> {
    let remote_schema = match obj.get("remote_schema") {
        Some(Json::String(s)) => s.clone(),
        Some(_) => return Err("remote_schema: expected a string".to_owned()),
        None => return Err("key \"remote_schema\" not found".to_owned()),
    };
    let lhs_fields = match obj.get(fields_key) {
        Some(Json::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("{fields_key}: expected an array of strings"))
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err(format!("{fields_key}: expected an array of strings")),
        None => return Err(format!("key \"{fields_key}\" not found")),
    };
    let hops = match obj.get("remote_field") {
        Some(v) => parse_hops(v)?,
        None => return Err("key \"remote_field\" not found".to_owned()),
    };
    Ok(ToRemoteSchema {
        remote_schema,
        lhs_fields,
        hops,
    })
}

fn parse_hops(mut v: &Json) -> Result<Vec<Hop>, String> {
    const SHAPE: &str =
        "Remote fields are represented by an object that maps each field name to its arguments.";
    let mut hops = vec![];
    loop {
        let Json::Object(o) = v else {
            return Err(SHAPE.to_owned());
        };
        let mut entries = o.iter();
        let (name, body) = match (entries.next(), entries.next()) {
            (None, _) => {
                return Err(format!(
                    "{SHAPE} Expecting one single mapping, received none."
                ));
            }
            (Some(e), None) => e,
            (Some(_), Some(_)) => {
                return Err(format!(
                    "{SHAPE} Expecting one single mapping, received too many."
                ));
            }
        };
        let Json::Object(body) = body else {
            return Err(format!("{name}: expected an object with \"arguments\""));
        };
        let arguments = match body.get("arguments") {
            Some(Json::Object(a)) => a.clone(),
            Some(_) => return Err(format!("{name}.arguments: expected an object")),
            None => return Err("key \"arguments\" not found".to_owned()),
        };
        hops.push(Hop {
            field: name.clone(),
            arguments,
        });
        match body.get("field") {
            None | Some(Json::Null) => return Ok(hops),
            Some(next) => v = next,
        }
    }
}

/// A join field of the table: what the database returns for it.
#[derive(Debug, Clone)]
pub enum JoinField {
    Column(ColumnType),
    /// A scalar computed field without input arguments.
    Computed(Arc<ComputedFieldInfo>),
}

/// A table's relationship to a remote schema, validated.
#[derive(Debug)]
pub struct RemoteRelationship {
    pub name: String,
    pub remote: Arc<RemoteSchemaInfo>,
    /// The join fields the templates use, in `lhs_fields` order.
    pub lhs: Vec<(String, JoinField)>,
    pub hops: Vec<Hop>,
    /// The field's type and description (the target type's), with customized names.
    pub ty: TypeRef,
    pub description: Option<String>,
    /// The leaf field's arguments the template leaves to the user.
    pub args: IndexMap<String, InputValueDef>,
    /// Input types cloned without their templated fields (§8.4 step 3).
    pub input_types: Vec<InputObjectDef>,
    /// Cloned input type → the remote's original name for it.
    pub clone_of: HashMap<String, String>,
}

impl RemoteRelationship {
    /// The remote's original name for a type of the Hasura schema.
    pub fn original_type<'a>(&'a self, name: &'a str) -> &'a str {
        match self.clone_of.get(name) {
            Some(original) => original,
            None => self.remote.original_type(name),
        }
    }
}

/// Validate the remote relationships of every table against the accepted remotes, adding
/// the valid ones to `remotes`. Returns the inconsistent ones.
pub fn resolve_table_relationships(
    tables: &Tables,
    remotes: &mut RemoteSchemas,
) -> Vec<InconsistentObject> {
    let mut issues = vec![];
    for table in &tables.ordered {
        for entry in &table.entry.remote_relationships {
            let name = entry.get("name").and_then(Json::as_str).unwrap_or_default();
            let def = entry.get("definition").unwrap_or(&Json::Null);
            let resolved = parse_definition(def, Format::Lenient).and_then(|d| match d {
                Definition::ToRemoteSchema(d) => resolve(table, name, &d, remotes).map(Some),
                // Not built yet: left out of the schema without an inconsistency, so
                // metadata V2 accepts still loads.
                Definition::ToSource => Ok(None),
            });
            match resolved {
                Ok(None) => {}
                Ok(Some(rel)) => remotes
                    .relationships
                    .entry((table.source.clone(), table.name.clone()))
                    .or_default()
                    .push(Arc::new(rel)),
                Err(msg) => issues.push(InconsistentObject::table_object(
                    "remote_relationship",
                    name,
                    &table.source,
                    &table.name,
                    format!(
                        "in table \"{}\": in remote relationship \"{name}\": {msg}",
                        table.name.v2_text()
                    ),
                    entry.clone(),
                )),
            }
        }
    }
    issues
}

/// The join fields of a table: its columns and its scalar computed fields without input
/// arguments (spec 08 §8.3).
fn join_fields(table: &TableInfo) -> Vec<(String, JoinField)> {
    let columns = table
        .columns
        .values()
        .map(|c| (c.name.clone(), JoinField::Column(c.ty.clone())));
    let computed = table
        .computed_fields
        .iter()
        .filter(|cf| matches!(cf.returns, ComputedReturn::Scalar(_)) && !cf.has_input_args())
        .map(|cf| (cf.name.clone(), JoinField::Computed(cf.clone())));
    columns.chain(computed).collect()
}

/// V2's conflict checks of a field name against the table's own fields.
fn field_conflict(table: &TableInfo, name: &str) -> Option<String> {
    if let Some(col) = table
        .columns
        .values()
        .find(|c| c.gql_name == name && c.name != name)
    {
        return Some(format!(
            "field definition conflicts with custom field name for postgres column \"{}\"",
            col.name
        ));
    }
    if table.columns.contains_key(name) {
        return Some("field definition conflicts with postgres column".to_owned());
    }
    let taken = table.relationship_by_name(name).is_some()
        || table.computed_fields.iter().any(|cf| cf.name == name);
    taken.then(|| format!("field \"{name}\" conflicts with an existing field of the table"))
}

/// `"a", "b" or "c"`
fn or_list(names: &[String]) -> String {
    let quoted: Vec<String> = names.iter().map(|n| format!("\"{n}\"")).collect();
    match quoted.split_last() {
        None => String::new(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
    }
}

fn is_graphql_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn resolve(
    table: &TableInfo,
    name: &str,
    def: &ToRemoteSchema,
    remotes: &RemoteSchemas,
) -> Result<RemoteRelationship, String> {
    if let Some(conflict) = field_conflict(table, name) {
        return Err(conflict);
    }
    // V2's order: the remote, then every join field's existence, then their names.
    let remote = remotes
        .schemas
        .iter()
        .find(|r| *r.name == *def.remote_schema)
        .ok_or_else(|| {
            format!(
                "remote schema with name \"{}\" not found",
                def.remote_schema
            )
        })?
        .clone();
    let available = join_fields(table);
    for f in &def.lhs_fields {
        if !available.iter().any(|(n, _)| n == f) {
            let names: Vec<String> = available.iter().map(|(n, _)| n.clone()).collect();
            return Err(format!(
                "field with name \"{f}\"is not provided by the lhs entity\"{}\"for defining a join condition, the allowed fields are {}",
                table.name.v2_text(),
                or_list(&names)
            ));
        }
    }
    if let Some(f) = def.lhs_fields.iter().find(|f| !is_graphql_name(f)) {
        return Err(format!("\"{f}\" is not a valid GraphQL identifier"));
    }

    // Walk the template from the query root, against the original types.
    let doc = &remote.original;
    let mut parent = doc.query_root.as_str();
    let last = def.hops.len().saturating_sub(1);
    let mut leaf_parent = parent;
    for (i, hop) in def.hops.iter().enumerate() {
        let field = doc
            .fields_of(parent)
            .and_then(|f| f.get(&hop.field))
            .ok_or_else(|| {
                format!(
                    "remote field with name \"{}\" and type \"{parent}\" not found",
                    hop.field
                )
            })?;
        for (k, v) in &hop.arguments {
            let arg = field
                .args
                .get(k)
                .ok_or_else(|| format!("argument with name \"{k}\" not found in remote schema"))?;
            validate_value(doc, k, v, &arg.ty, &def.lhs_fields)?;
        }
        let base = field.ty.base();
        if i < last {
            match doc.types.get_key_value(base) {
                Some((name, TypeDef::Object(_))) => parent = name,
                _ => return Err(format!("type \"{base}\"only output type is expected")),
            }
        } else {
            leaf_parent = parent;
        }
    }
    let leaf = def.hops.last().ok_or("remote_field: no field given")?;

    // The leaf field as the Hasura schema shows it.
    let custom_parent = remote
        .to_custom
        .get(leaf_parent)
        .map_or(leaf_parent, String::as_str);
    let field = remote
        .customized
        .fields_of(custom_parent)
        .and_then(|fields| {
            fields.values().find(|f| match &f.kind {
                FieldKind::Remote(RemoteField::Field { original }) => *original == leaf.field,
                _ => f.name == leaf.field,
            })
        })
        .ok_or_else(|| format!("remote field with name \"{}\" not found", leaf.field))?;
    let description = remote
        .customized
        .get(field.ty.base())
        .and_then(|t| t.description())
        .map(str::to_owned);

    let lhs_id: String = table
        .name
        .v2_text()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let mut stripper = Stripper {
        remote: &remote,
        suffix: format!("_remote_rel_{lhs_id}{name}"),
        clones: vec![],
        clone_of: HashMap::new(),
    };
    let args = stripper.strip(&field.args, &leaf.arguments);
    // Every declared join field takes part, used by the template or not (V2's
    // `requiredLHSJoinFields`).
    let lhs = def
        .lhs_fields
        .iter()
        .filter_map(|f| available.iter().find(|(n, _)| n == f).cloned())
        .collect();
    Ok(RemoteRelationship {
        name: name.to_owned(),
        lhs,
        hops: def.hops.clone(),
        ty: field.ty.clone(),
        description,
        args,
        input_types: stripper.clones,
        clone_of: stripper.clone_of,
        remote,
    })
}

/// The type without its non-null wrapper.
fn nullable(ty: &TypeRef) -> &TypeRef {
    match ty {
        TypeRef::NonNull(t) => t,
        t => t,
    }
}

/// V2's `validateType` for one template value against the argument's type.
fn validate_value(
    doc: &super::RemoteDocument,
    key: &str,
    v: &Json,
    expected: &TypeRef,
    lhs: &[String],
) -> Result<(), String> {
    match v {
        Json::String(s) if s.starts_with('$') => {
            let var = &s[1..];
            if var.is_empty() {
                return Err("Empty variable name".to_owned());
            }
            if !lhs.iter().any(|f| f == var) {
                return Err(format!("variable \"{var}\" is not found"));
            }
            Ok(())
        }
        Json::String(_) => coercible("String", expected),
        Json::Number(n) => coercible(
            if n.is_i64() || n.is_u64() {
                "Int"
            } else {
                "Float"
            },
            expected,
        ),
        Json::Bool(_) => coercible("Boolean", expected),
        Json::Null => Err("null is not allowed here".to_owned()),
        Json::Array(items) => {
            if items.len() > 1 {
                return Err("multiple elements in list value is not supported".to_owned());
            }
            let TypeRef::List(inner) = nullable(expected) else {
                return Err(format!("type \"{}\"is not a list type", expected.base()));
            };
            match items.first() {
                Some(item) => validate_value(doc, key, item, inner, lhs),
                None => Ok(()),
            }
        }
        Json::Object(o) if o.is_empty() => Ok(()),
        Json::Object(o) => {
            let base = expected.base();
            let fields = match doc.get(base) {
                Some(TypeDef::InputObject(io)) => &io.fields,
                Some(_) => return Err(format!("type \"{key}\"not an input object type")),
                None => return Err(format!("type with name \"{base}\" not found")),
            };
            for (k, v) in o {
                let f = fields.get(k).ok_or_else(|| {
                    format!("argument with name \"{k}\" not found in remote schema")
                })?;
                validate_value(doc, k, v, &f.ty, lhs)?;
            }
            Ok(())
        }
    }
}

/// `isTypeCoercible` for a literal of scalar type `actual`.
fn coercible(actual: &str, expected: &TypeRef) -> Result<(), String> {
    let base = expected.base();
    if base == "ID" {
        if ["ID", "Int", "String", "bigint", "smallint", "uuid"].contains(&actual) {
            return Ok(());
        }
        return Err(format!(
            "Only ID, Int, uuid or String scalar types can be joined to the ID type, but received \"{actual}\""
        ));
    }
    if base == actual {
        Ok(())
    } else {
        Err(format!("expected type \"{base}\" but got \"{actual}\""))
    }
}

/// A template value fixed entirely by the template (V2's atomic values).
fn is_atomic(v: &Json) -> bool {
    match v {
        Json::Object(_) => false,
        Json::Array(items) => items.first().is_none_or(is_atomic),
        _ => true,
    }
}

/// The type with its named type replaced.
fn with_base(ty: &TypeRef, name: &str) -> TypeRef {
    match ty {
        TypeRef::Named(_) => TypeRef::named(name),
        TypeRef::List(t) => TypeRef::List(Box::new(with_base(t, name))),
        TypeRef::NonNull(t) => TypeRef::NonNull(Box::new(with_base(t, name))),
    }
}

/// Removes templated arguments, cloning input types whose fields are partly templated.
struct Stripper<'a> {
    remote: &'a RemoteSchemaInfo,
    suffix: String,
    clones: Vec<InputObjectDef>,
    clone_of: HashMap<String, String>,
}

impl Stripper<'_> {
    fn strip(
        &mut self,
        args: &IndexMap<String, InputValueDef>,
        template: &Map<String, Json>,
    ) -> IndexMap<String, InputValueDef> {
        let mut out = IndexMap::new();
        for (name, def) in args {
            let object = match template.get(name) {
                None => {
                    out.insert(name.clone(), def.clone());
                    continue;
                }
                Some(v) if is_atomic(v) => continue,
                Some(Json::Object(o)) => o,
                Some(Json::Array(items)) => match items.first() {
                    Some(Json::Object(o)) => o,
                    _ => continue,
                },
                Some(_) => continue,
            };
            let clone = self.clone_input(def.ty.base(), object);
            out.insert(
                name.clone(),
                InputValueDef {
                    ty: with_base(&def.ty, &clone),
                    ..def.clone()
                },
            );
        }
        out
    }

    /// `<Type>_remote_rel_<lhs><rel>`: the input type without the templated fields.
    fn clone_input(&mut self, name: &str, template: &Map<String, Json>) -> String {
        let Some(TypeDef::InputObject(io)) = self.remote.customized.get(name) else {
            return name.to_owned();
        };
        let new_name = format!("{name}{}", self.suffix);
        if self.clone_of.contains_key(&new_name) {
            return new_name;
        }
        self.clone_of
            .insert(new_name.clone(), self.remote.original_type(name).to_owned());
        let fields = self.strip(&io.fields, template);
        self.clones.push(InputObjectDef {
            name: new_name.clone(),
            description: io.description.clone(),
            fields,
        });
        new_name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_legacy_and_unified_formats() {
        let legacy = json!({"remote_schema": "r", "hasura_fields": ["id"],
            "remote_field": {"message": {"arguments": {"id": "$id"}}}});
        let unified = json!({"to_remote_schema": {"remote_schema": "r", "lhs_fields": ["id"],
            "remote_field": {"message": {"arguments": {"id": "$id"}}}}});
        let a = parse_definition(&legacy, Format::Lenient).unwrap();
        assert_eq!(a, parse_definition(&unified, Format::Lenient).unwrap());
        assert_eq!(
            parse_definition(&legacy, Format::Strict).unwrap_err(),
            "remote relationship definition (strict format) expects exactly one of: to_source, to_remote_schema"
        );
        let nested = json!({"remote_schema": "r", "hasura_fields": [],
            "remote_field": {"user": {"arguments": {}, "field": {"messages": {"arguments": {}}}}}});
        let Definition::ToRemoteSchema(d) = parse_definition(&nested, Format::Lenient).unwrap()
        else {
            panic!()
        };
        assert_eq!(d.hops.len(), 2);
        let two = json!({"remote_schema": "r", "hasura_fields": [],
            "remote_field": {"a": {"arguments": {}}, "b": {"arguments": {}}}});
        assert!(
            parse_definition(&two, Format::Lenient)
                .unwrap_err()
                .ends_with("received too many.")
        );
    }

    #[test]
    fn literal_coercion_follows_v2() {
        assert!(coercible("Int", &TypeRef::nn("ID")).is_ok());
        assert_eq!(
            coercible("Boolean", &TypeRef::nn("ID")).unwrap_err(),
            "Only ID, Int, uuid or String scalar types can be joined to the ID type, but received \"Boolean\""
        );
        assert_eq!(
            coercible("String", &TypeRef::nn("Int")).unwrap_err(),
            "expected type \"Int\" but got \"String\""
        );
        assert_eq!(or_list(&["id".into(), "name".into()]), "\"id\" or \"name\"");
        assert_eq!(
            or_list(&["a".into(), "b".into(), "c".into()]),
            "\"a\", \"b\" or \"c\""
        );
    }
}
