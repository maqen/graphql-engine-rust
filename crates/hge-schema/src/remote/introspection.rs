//! Parsing a remote server's introspection result into schema types (spec 08 §3.3) and
//! validating it (§3.4).

use indexmap::IndexMap;
use serde_json::{Map, Value as Json};

use super::RemoteField;
use crate::model::*;

/// A remote schema as introspected, with its original names.
#[derive(Debug, Clone)]
pub struct RemoteDocument {
    pub query_root: String,
    pub mutation_root: Option<String>,
    pub subscription_root: Option<String>,
    /// Every type the remote declares, built-in scalars included.
    pub types: IndexMap<String, TypeDef>,
}

impl RemoteDocument {
    pub fn get(&self, name: &str) -> Option<&TypeDef> {
        self.types.get(name)
    }

    pub fn fields_of(&self, name: &str) -> Option<&IndexMap<String, FieldDef>> {
        match self.types.get(name) {
            Some(TypeDef::Object(o)) => Some(&o.fields),
            Some(TypeDef::Interface(i)) => Some(&i.fields),
            _ => None,
        }
    }

    /// The root types: query, then mutation and subscription when present.
    pub fn roots(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.query_root.as_str())
            .chain(self.mutation_root.as_deref())
            .chain(self.subscription_root.as_deref())
    }
}

pub const BUILTIN_SCALARS: [&str; 5] = ["Boolean", "Float", "ID", "Int", "String"];

/// The query Hasura sends to introspect a remote schema: V2's `introspection.json` with the
/// fragments inlined, as V2's printer sends it.
pub fn introspection_query() -> &'static str {
    use super::print::{OutField, OutOperation, OutSelection, operation};
    static QUERY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    QUERY.get_or_init(|| {
        fn f(name: &str, selection: Vec<OutSelection>) -> OutSelection {
            OutSelection::Field(OutField {
                selection,
                ..OutField::new(name)
            })
        }
        fn leaves(names: &[&str]) -> Vec<OutSelection> {
            names.iter().map(|n| f(n, vec![])).collect()
        }
        fn with_deprecated(name: &str, selection: Vec<OutSelection>) -> OutSelection {
            OutSelection::Field(OutField {
                args: vec![(
                    "includeDeprecated".into(),
                    async_graphql_value::Value::Boolean(true),
                )],
                selection,
                ..OutField::new(name)
            })
        }
        // `TypeRef`: kind, name and `ofType` nested seven times.
        fn type_ref(depth: usize) -> Vec<OutSelection> {
            let mut sel = leaves(&["kind", "name"]);
            if depth > 0 {
                sel.push(f("ofType", type_ref(depth - 1)));
            }
            sel
        }
        let input_value = || {
            let mut sel = leaves(&["name", "description"]);
            sel.push(f("type", type_ref(7)));
            sel.push(f("defaultValue", vec![]));
            sel
        };
        let full_type = vec![
            f("kind", vec![]),
            f("name", vec![]),
            f("description", vec![]),
            with_deprecated(
                "fields",
                vec![
                    f("name", vec![]),
                    f("description", vec![]),
                    f("args", input_value()),
                    f("type", type_ref(7)),
                    f("isDeprecated", vec![]),
                    f("deprecationReason", vec![]),
                ],
            ),
            f("inputFields", input_value()),
            f("interfaces", type_ref(7)),
            with_deprecated(
                "enumValues",
                leaves(&["name", "description", "isDeprecated", "deprecationReason"]),
            ),
            f("possibleTypes", type_ref(7)),
        ];
        let mut directive = leaves(&["name", "description", "locations"]);
        directive.push(f("args", input_value()));
        let schema = vec![
            f("queryType", leaves(&["name"])),
            f("mutationType", leaves(&["name"])),
            f("subscriptionType", leaves(&["name"])),
            f("types", full_type),
            f("directives", directive),
        ];
        operation(&OutOperation {
            kind: "query",
            name: Some("IntrospectionQuery"),
            variables: vec![],
            selection: vec![f("__schema", schema)],
        })
    })
}

/// The request body for [`introspection_query`], keys in V2's order.
pub fn introspection_request_body() -> String {
    serde_json::json!({
        "operationName": "IntrospectionQuery",
        "query": introspection_query(),
    })
    .to_string()
}

/// A parse failure, reported like Aeson: `Error in $.types[1].name: ...`.
fn err(path: &str, msg: impl std::fmt::Display) -> String {
    format!("Error in {path}: {msg}")
}

pub(super) fn kind_of(v: &Json) -> &'static str {
    match v {
        Json::Null => "Null",
        Json::Bool(_) => "Boolean",
        Json::Number(_) => "Number",
        Json::String(_) => "String",
        Json::Array(_) => "Array",
        Json::Object(_) => "Object",
    }
}

fn obj<'a>(v: &'a Json, path: &str) -> Result<&'a Map<String, Json>, String> {
    v.as_object().ok_or_else(|| {
        err(
            path,
            format!(
                "parsing Object failed, expected Object, but encountered {}",
                kind_of(v)
            ),
        )
    })
}

fn field<'a>(o: &'a Map<String, Json>, key: &str, path: &str) -> Result<&'a Json, String> {
    o.get(key)
        .ok_or_else(|| err(path, format!("key \"{key}\" not found")))
}

fn opt_field<'a>(o: &'a Map<String, Json>, key: &str) -> Option<&'a Json> {
    o.get(key).filter(|v| !v.is_null())
}

fn name(v: &Json, path: &str) -> Result<String, String> {
    match v {
        Json::String(s) if is_valid_name(s) => Ok(s.clone()),
        Json::String(s) => Err(err(path, format!("invalid GraphQL name: {s:?}"))),
        other => Err(err(
            path,
            format!(
                "parsing Name failed, expected String, but encountered {}",
                kind_of(other)
            ),
        )),
    }
}

pub fn is_valid_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn description(o: &Map<String, Json>, path: &str) -> Result<Option<String>, String> {
    match opt_field(o, "description") {
        None => Ok(None),
        Some(Json::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(err(
            &format!("{path}.description"),
            format!(
                "parsing Text failed, expected String, but encountered {}",
                kind_of(other)
            ),
        )),
    }
}

fn array<'a>(v: &'a Json, path: &str) -> Result<&'a Vec<Json>, String> {
    v.as_array().ok_or_else(|| {
        err(
            path,
            format!(
                "parsing [] failed, expected Array, but encountered {}",
                kind_of(v)
            ),
        )
    })
}

/// Parse `{"data": {"__schema": ...}}`.
pub fn parse_introspection(body: &Json) -> Result<RemoteDocument, String> {
    let root = obj(body, "$")?;
    let data = obj(field(root, "data", "$")?, "$.data")?;
    let schema = obj(field(data, "__schema", "$.data")?, "$.data.__schema")?;
    parse_schema(schema)
}

fn root_name(schema: &Map<String, Json>, key: &str) -> Result<Option<String>, String> {
    match opt_field(schema, key) {
        None => Ok(None),
        Some(v) => {
            let path = format!("$.{key}");
            let o = obj(v, &path)?;
            Ok(Some(name(
                field(o, "name", &path)?,
                &format!("{path}.name"),
            )?))
        }
    }
}

fn parse_schema(schema: &Map<String, Json>) -> Result<RemoteDocument, String> {
    let query_root = match root_name(schema, "queryType")? {
        Some(n) => n,
        None => return Err(err("$", "key \"queryType\" not found")),
    };
    let mutation_root = root_name(schema, "mutationType")?;
    let subscription_root = root_name(schema, "subscriptionType")?;
    let mut types = IndexMap::new();
    for (i, t) in array(field(schema, "types", "$")?, "$.types")?
        .iter()
        .enumerate()
    {
        let ty = parse_type(t, &format!("$.types[{i}]"))?;
        types.insert(ty.name().to_owned(), ty);
    }
    for s in BUILTIN_SCALARS {
        types.entry(s.to_owned()).or_insert_with(|| {
            TypeDef::Scalar(ScalarDef {
                name: s.to_owned(),
                description: None,
            })
        });
    }
    // `addDefaultRoots`: objects named `Mutation` / `Subscription` are roots by default.
    let default_root = |n: &str| match types.get(n) {
        Some(TypeDef::Object(_)) => Some(n.to_owned()),
        _ => None,
    };
    let mutation_root = mutation_root.or_else(|| default_root("Mutation"));
    let subscription_root = subscription_root.or_else(|| default_root("Subscription"));
    Ok(RemoteDocument {
        query_root,
        mutation_root,
        subscription_root,
        types,
    })
}

fn parse_type(v: &Json, path: &str) -> Result<TypeDef, String> {
    let o = obj(v, path)?;
    let kind = match field(o, "kind", path)? {
        Json::String(s) => s.as_str(),
        other => {
            return Err(err(
                &format!("{path}.kind"),
                format!(
                    "parsing Text failed, expected String, but encountered {}",
                    kind_of(other)
                ),
            ));
        }
    };
    let type_name = name(field(o, "name", path)?, &format!("{path}.name"))?;
    let type_description = description(o, path)?;
    let refs = |key: &str, required: bool| -> Result<Vec<(String, String)>, String> {
        let list = match opt_field(o, key) {
            Some(v) => array(v, &format!("{path}.{key}"))?,
            None if required => return Err(err(path, format!("key \"{key}\" not found"))),
            None => return Ok(vec![]),
        };
        list.iter()
            .enumerate()
            .map(|(i, r)| {
                let p = format!("{path}.{key}[{i}]");
                let ro = obj(r, &p)?;
                let n = name(field(ro, "name", &p)?, &format!("{p}.name"))?;
                let k = field(ro, "kind", &p)?
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                Ok((k, n))
            })
            .collect()
    };
    Ok(match kind {
        "SCALAR" => TypeDef::Scalar(ScalarDef {
            name: type_name,
            description: type_description,
        }),
        "OBJECT" => {
            let mut interfaces = vec![];
            for (k, n) in refs("interfaces", false)? {
                if k != "INTERFACE" {
                    return Err(format!(
                        "Error: object type {type_name} can only implement interfaces"
                    ));
                }
                interfaces.push(n);
            }
            TypeDef::Object(ObjectDef {
                name: type_name,
                description: type_description,
                fields: parse_fields(o, path)?,
                interfaces,
            })
        }
        "INTERFACE" => {
            let mut possible_types = vec![];
            for (k, n) in refs("possibleTypes", false)? {
                if k != "OBJECT" {
                    return Err(format!(
                        "Error: interface type {type_name} can only be implemented by objects"
                    ));
                }
                possible_types.push(n);
            }
            TypeDef::Interface(InterfaceDef {
                name: type_name,
                description: type_description,
                fields: parse_fields(o, path)?,
                possible_types,
            })
        }
        "UNION" => {
            let mut possible_types = vec![];
            for (k, n) in refs("possibleTypes", true)? {
                if k != "OBJECT" {
                    return Err(format!(
                        "Error: union type {type_name} can only be implemented by objects"
                    ));
                }
                possible_types.push(n);
            }
            TypeDef::Union(UnionDef {
                name: type_name,
                description: type_description,
                possible_types,
            })
        }
        "ENUM" => {
            let values = array(field(o, "enumValues", path)?, &format!("{path}.enumValues"))?;
            let mut out = IndexMap::new();
            for (i, v) in values.iter().enumerate() {
                let p = format!("{path}.enumValues[{i}]");
                let vo = obj(v, &p)?;
                let n = name(field(vo, "name", &p)?, &format!("{p}.name"))?;
                out.insert(
                    n.clone(),
                    EnumValueDef {
                        name: n,
                        description: description(vo, &p)?,
                        deprecation_reason: None,
                    },
                );
            }
            TypeDef::Enum(EnumDef {
                name: type_name,
                description: type_description,
                values: out,
            })
        }
        "INPUT_OBJECT" => {
            let mut fields = IndexMap::new();
            if let Some(list) = opt_field(o, "inputFields") {
                for (i, f) in array(list, &format!("{path}.inputFields"))?
                    .iter()
                    .enumerate()
                {
                    let iv = parse_input_value(f, &format!("{path}.inputFields[{i}]"))?;
                    fields.insert(iv.name.clone(), iv);
                }
            }
            TypeDef::InputObject(InputObjectDef {
                name: type_name,
                description: type_description,
                fields,
            })
        }
        other => return Err(err(path, format!("unknown kind: {other}"))),
    })
}

fn parse_fields(o: &Map<String, Json>, path: &str) -> Result<IndexMap<String, FieldDef>, String> {
    let mut fields = IndexMap::new();
    let Some(list) = opt_field(o, "fields") else {
        return Ok(fields);
    };
    for (i, f) in array(list, &format!("{path}.fields"))?.iter().enumerate() {
        let p = format!("{path}.fields[{i}]");
        let fo = obj(f, &p)?;
        let n = name(field(fo, "name", &p)?, &format!("{p}.name"))?;
        let ty = parse_type_ref(field(fo, "type", &p)?, &format!("{p}.type"))?;
        let mut def = FieldDef::new(
            n.clone(),
            ty,
            FieldKind::Remote(RemoteField::Field {
                original: n.clone(),
            }),
        )
        .describe_opt(description(fo, &p)?);
        for (j, a) in array(field(fo, "args", &p)?, &format!("{p}.args"))?
            .iter()
            .enumerate()
        {
            def = def.arg(parse_input_value(a, &format!("{p}.args[{j}]"))?);
        }
        fields.insert(n, def);
    }
    Ok(fields)
}

fn parse_input_value(v: &Json, path: &str) -> Result<InputValueDef, String> {
    let o = obj(v, path)?;
    let n = name(field(o, "name", path)?, &format!("{path}.name"))?;
    let ty = parse_type_ref(field(o, "type", path)?, &format!("{path}.type"))?;
    let default_value = match opt_field(o, "defaultValue") {
        None => None,
        Some(Json::String(s)) => {
            let parsed = parse_const_value(s)
                .ok_or_else(|| err(&format!("{path}.defaultValue"), "invalid GraphQL value"))?;
            Some(super::print::const_value(&parsed))
        }
        Some(other) => {
            return Err(err(
                &format!("{path}.defaultValue"),
                format!(
                    "parsing Text failed, expected String, but encountered {}",
                    kind_of(other)
                ),
            ));
        }
    };
    Ok(InputValueDef {
        name: n,
        description: description(o, path)?,
        ty,
        default_value,
        parser: ValueParser::Strict,
    })
}

/// Parse a GraphQL constant value (a default value from introspection).
pub fn parse_const_value(text: &str) -> Option<async_graphql_value::ConstValue> {
    // The parser only parses documents; wrap the value in a query argument.
    let doc = async_graphql_parser::parse_query(format!("{{ f(v: {text}) }}")).ok()?;
    let op = match doc.operations {
        async_graphql_parser::types::DocumentOperations::Single(op) => op,
        _ => return None,
    };
    let sel = op.node.selection_set.node.items.into_iter().next()?;
    let async_graphql_parser::types::Selection::Field(f) = sel.node else {
        return None;
    };
    let (_, v) = f.node.arguments.into_iter().next()?;
    v.node.into_const()
}

fn parse_type_ref(v: &Json, path: &str) -> Result<TypeRef, String> {
    let o = obj(v, path)?;
    let kind = field(o, "kind", path)?.as_str().unwrap_or_default();
    match kind {
        "NON_NULL" | "LIST" => {
            let inner = parse_type_ref(field(o, "ofType", path)?, &format!("{path}.ofType"))?;
            Ok(if kind == "LIST" {
                inner.list()
            } else {
                // `[T]!!` cannot happen; keep the wrapper as given.
                TypeRef::NonNull(Box::new(inner))
            })
        }
        _ => Ok(TypeRef::Named(name(
            field(o, "name", path)?,
            &format!("{path}.name"),
        )?)),
    }
}

/// `stitchRemoteSchema` checks (spec 08 §3.4), on the original document. Only types
/// reachable from the root types are checked, like V2's parser-driven validation.
pub fn validate(doc: &RemoteDocument) -> Result<(), String> {
    for root in doc.roots() {
        match doc.get(root) {
            Some(TypeDef::Object(_)) => {}
            Some(_) => return Err(format!("\"{root}\" has to be an object type")),
            None => return Err(format!("Could not find type with name \"{root}\"")),
        }
    }
    let mut seen = std::collections::HashSet::new();
    let mut queue: Vec<&str> = doc.roots().collect();
    while let Some(name) = queue.pop() {
        if !seen.insert(name) {
            continue;
        }
        let ty = doc
            .get(name)
            .ok_or_else(|| format!("Could not find type with name \"{name}\""))?;
        match ty {
            TypeDef::Object(o) => {
                for f in o.fields.values() {
                    for a in f.args.values() {
                        input(doc, &a.ty, &mut queue)?;
                    }
                }
                for i in &o.interfaces {
                    let Some(TypeDef::Interface(iface)) = doc.get(i) else {
                        return Err(format!(
                            "Could not find interface '{i}' implemented by Object type '{}'",
                            o.name
                        ));
                    };
                    check_implementation(doc, o, iface)?;
                }
                for f in o.fields.values() {
                    output(doc, &f.ty, &mut queue)?;
                }
            }
            TypeDef::Interface(i) => {
                if i.fields.is_empty() {
                    return Err(format!(
                        "List of fields cannot be empty for interface '{}'",
                        i.name
                    ));
                }
                for f in i.fields.values() {
                    for a in f.args.values() {
                        input(doc, &a.ty, &mut queue)?;
                    }
                }
                for f in i.fields.values() {
                    output(doc, &f.ty, &mut queue)?;
                }
                members(doc, "Interface", &i.name, &i.possible_types, &mut queue)?;
            }
            TypeDef::Union(u) => {
                if u.possible_types.is_empty() {
                    return Err(format!(
                        "List of member types cannot be empty for union type '{}'",
                        u.name
                    ));
                }
                members(doc, "Union", &u.name, &u.possible_types, &mut queue)?;
            }
            TypeDef::InputObject(io) => {
                for f in io.fields.values() {
                    input(doc, &f.ty, &mut queue)?;
                }
            }
            TypeDef::Scalar(_) | TypeDef::Enum(_) => {}
        }
    }
    Ok(())
}

fn input<'a>(
    doc: &'a RemoteDocument,
    ty: &TypeRef,
    queue: &mut Vec<&'a str>,
) -> Result<(), String> {
    match doc.get(ty.base()) {
        None => Err(format!("Could not find type with name \"{}\"", ty.base())),
        Some(t) if !t.is_input() => Err("expected input type, but got output type".into()),
        Some(t) => {
            queue.push(t.name());
            Ok(())
        }
    }
}

fn output<'a>(
    doc: &'a RemoteDocument,
    ty: &TypeRef,
    queue: &mut Vec<&'a str>,
) -> Result<(), String> {
    match doc.get(ty.base()) {
        None => Err(format!("Could not find type with name \"{}\"", ty.base())),
        Some(TypeDef::InputObject(_)) => Err("expected output type, but got input type".into()),
        Some(t) => {
            queue.push(t.name());
            Ok(())
        }
    }
}

fn members<'a>(
    doc: &'a RemoteDocument,
    kind: &str,
    name: &str,
    possible: &[String],
    queue: &mut Vec<&'a str>,
) -> Result<(), String> {
    for m in possible {
        match doc.get(m) {
            None => {
                return Err(format!(
                    "Could not find type '{m}', which is defined as a member type of {kind} '{name}'"
                ));
            }
            Some(TypeDef::Object(o)) => queue.push(&o.name),
            Some(_) => {
                return Err(format!(
                    "{kind} type '{name}' can only include object types. It cannot include '{m}'"
                ));
            }
        }
    }
    Ok(())
}

fn check_implementation(
    doc: &RemoteDocument,
    o: &ObjectDef,
    iface: &InterfaceDef,
) -> Result<(), String> {
    for (fname, ifield) in &iface.fields {
        let Some(ofield) = o.fields.get(fname) else {
            return Err(format!(
                "Interface field '{}'.\"{fname}\" expected, but '{}' does not provide it",
                iface.name, o.name
            ));
        };
        if !is_subtype(doc, &ofield.ty, &ifield.ty) {
            return Err(format!(
                "The type of Object field '{}'.\"{fname}\" ({}) is not the same type/sub type of Interface field '{}'.\"{fname}\" ({})",
                o.name, ofield.ty, iface.name, ifield.ty
            ));
        }
        for (aname, iarg) in &ifield.args {
            match ofield.args.get(aname) {
                None => {
                    return Err(format!(
                        "Interface field argument '{}'.\"{fname}\"(\"{aname}\":) required, but Object field '{}'.\"{fname}\" does not provide it",
                        iface.name, o.name
                    ));
                }
                Some(oarg) if oarg.ty != iarg.ty => {
                    return Err(format!(
                        "Interface field argument '{}'.\"{fname}\"(\"{aname}\":) expects type {}, but '{}'.\"{fname}\"(\"{aname}\":) has type {}",
                        iface.name, iarg.ty, o.name, oarg.ty
                    ));
                }
                Some(_) => {}
            }
        }
        for (aname, oarg) in &ofield.args {
            if !ifield.args.contains_key(aname) && oarg.ty.is_non_null() {
                return Err(format!(
                    "Object field argument '{}'.\"{fname}\"(\"{aname}\":) is of required type {}, but is not provided by Interface field '{}'.\"{fname}\"",
                    o.name, oarg.ty, iface.name
                ));
            }
        }
    }
    Ok(())
}

/// `sub` may be used where `sup` is declared (interface field covariance).
fn is_subtype(doc: &RemoteDocument, sub: &TypeRef, sup: &TypeRef) -> bool {
    match (sub, sup) {
        (TypeRef::NonNull(a), TypeRef::NonNull(b)) => is_subtype(doc, a, b),
        (TypeRef::NonNull(a), b) => is_subtype(doc, a, b),
        (_, TypeRef::NonNull(_)) => false,
        (TypeRef::List(a), TypeRef::List(b)) => is_subtype(doc, a, b),
        (TypeRef::Named(a), TypeRef::Named(b)) => {
            a == b
                || match doc.get(b) {
                    Some(TypeDef::Interface(i)) => i.possible_types.contains(a),
                    Some(TypeDef::Union(u)) => u.possible_types.contains(a),
                    _ => false,
                }
        }
        _ => false,
    }
}
