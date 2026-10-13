//! Merges `actions.graphql` into the `actions` / `custom_types` from `actions.yaml`, the way the
//! Hasura CLI does when it builds metadata from a project directory.
//!
//! `actions.yaml` holds handler/kind/permissions/relationships; `actions.graphql` holds the
//! action signatures (`type Mutation { login(...): LoginOutput }`) and the custom types.

use async_graphql_parser::types::{
    InputValueDefinition, TypeDefinition, TypeKind, TypeSystemDefinition,
};
use async_graphql_parser::{Positioned, parse_schema};
use serde_json::{Map, Value, json};

pub fn merge(actions: &mut Vec<Value>, custom_types: &mut Value, sdl: &str) -> Result<(), String> {
    if sdl.trim().is_empty() {
        return Ok(());
    }
    let doc = parse_schema(sdl).map_err(|e| e.to_string())?;

    let mut enums = vec![];
    let mut input_objects = vec![];
    let mut objects = vec![];
    let mut scalars = vec![];

    for def in doc.definitions {
        let TypeSystemDefinition::Type(ty) = def else {
            continue;
        };
        let ty: TypeDefinition = ty.node;
        let name = ty.name.node.to_string();
        let description = ty.description.as_ref().map(|d| d.node.clone());
        match ty.kind {
            TypeKind::Object(obj) if name == "Query" || name == "Mutation" => {
                let op_type = if name == "Query" { "query" } else { "mutation" };
                for field in obj.fields {
                    let field = field.node;
                    let action_name = field.name.node.to_string();
                    let arguments: Vec<Value> = field.arguments.iter().map(argument).collect();
                    let output_type = field.ty.node.to_string();
                    let action = find_or_insert_action(actions, &action_name);
                    let def = action
                        .as_object_mut()
                        .expect("action is an object")
                        .entry("definition")
                        .or_insert_with(|| Value::Object(Map::new()));
                    if let Value::Object(def) = def {
                        def.insert("type".into(), Value::String(op_type.into()));
                        def.insert("arguments".into(), Value::Array(arguments));
                        def.insert("output_type".into(), Value::String(output_type));
                        def.entry("kind")
                            .or_insert_with(|| Value::String("synchronous".into()));
                    }
                    if let (Some(desc), Some(obj)) =
                        (field.description.as_ref(), action.as_object_mut())
                    {
                        obj.entry("comment")
                            .or_insert_with(|| Value::String(desc.node.clone()));
                    }
                }
            }
            TypeKind::Object(obj) => {
                let fields: Vec<Value> = obj
                    .fields
                    .iter()
                    .map(|f| {
                        let f = &f.node;
                        let mut out = Map::new();
                        out.insert("name".into(), Value::String(f.name.node.to_string()));
                        out.insert("type".into(), Value::String(f.ty.node.to_string()));
                        if let Some(d) = &f.description {
                            out.insert("description".into(), Value::String(d.node.clone()));
                        }
                        if !f.arguments.is_empty() {
                            out.insert(
                                "arguments".into(),
                                Value::Array(f.arguments.iter().map(argument).collect()),
                            );
                        }
                        Value::Object(out)
                    })
                    .collect();
                objects.push(type_def(&name, description, "fields", fields));
            }
            TypeKind::InputObject(obj) => {
                let fields: Vec<Value> = obj.fields.iter().map(argument).collect();
                input_objects.push(type_def(&name, description, "fields", fields));
            }
            TypeKind::Enum(en) => {
                let values: Vec<Value> = en
                    .values
                    .iter()
                    .map(|v| {
                        let v = &v.node;
                        let mut out = Map::new();
                        out.insert("value".into(), Value::String(v.value.node.to_string()));
                        out.insert(
                            "description".into(),
                            v.description
                                .as_ref()
                                .map(|d| Value::String(d.node.clone()))
                                .unwrap_or(Value::Null),
                        );
                        out.insert(
                            "is_deprecated".into(),
                            Value::Bool(
                                v.directives
                                    .iter()
                                    .any(|d| d.node.name.node.as_str() == "deprecated"),
                            ),
                        );
                        Value::Object(out)
                    })
                    .collect();
                enums.push(type_def(&name, description, "values", values));
            }
            TypeKind::Scalar => {
                let mut out = Map::new();
                out.insert("name".into(), Value::String(name));
                if let Some(d) = description {
                    out.insert("description".into(), Value::String(d));
                }
                scalars.push(Value::Object(out));
            }
            TypeKind::Interface(_) | TypeKind::Union(_) => {}
        }
    }

    if !custom_types.is_object() {
        *custom_types = Value::Object(Map::new());
    }
    let ct = custom_types.as_object_mut().expect("object");
    merge_types(ct, "enums", enums);
    merge_types(ct, "input_objects", input_objects);
    merge_types(ct, "objects", objects);
    merge_types(ct, "scalars", scalars);
    Ok(())
}

fn argument(arg: &Positioned<InputValueDefinition>) -> Value {
    let arg = &arg.node;
    let mut out = Map::new();
    out.insert("name".into(), Value::String(arg.name.node.to_string()));
    out.insert("type".into(), Value::String(arg.ty.node.to_string()));
    if let Some(d) = &arg.description {
        out.insert("description".into(), Value::String(d.node.clone()));
    }
    Value::Object(out)
}

fn type_def(name: &str, description: Option<String>, key: &str, items: Vec<Value>) -> Value {
    let mut out = Map::new();
    out.insert("name".into(), Value::String(name.to_owned()));
    if let Some(d) = description {
        out.insert("description".into(), Value::String(d));
    }
    out.insert(key.into(), Value::Array(items));
    Value::Object(out)
}

fn find_or_insert_action<'a>(actions: &'a mut Vec<Value>, name: &str) -> &'a mut Value {
    let pos = actions
        .iter()
        .position(|a| a.get("name").and_then(Value::as_str) == Some(name));
    match pos {
        Some(i) => &mut actions[i],
        None => {
            actions.push(json!({ "name": name, "definition": {} }));
            actions.last_mut().expect("just pushed")
        }
    }
}

/// SDL definitions win for shape; `actions.yaml` entries contribute everything else
/// (relationships, descriptions). Types only listed in the YAML are kept.
fn merge_types(ct: &mut Map<String, Value>, key: &str, from_sdl: Vec<Value>) {
    let existing = match ct.remove(key) {
        Some(Value::Array(a)) => a,
        _ => vec![],
    };
    let mut out: Vec<Value> = Vec::with_capacity(from_sdl.len().max(existing.len()));
    for mut sdl_ty in from_sdl {
        let name = sdl_ty.get("name").cloned();
        if let Some(yaml_ty) = existing.iter().find(|t| t.get("name") == name.as_ref())
            && let (Value::Object(sdl_obj), Value::Object(yaml_obj)) = (&mut sdl_ty, yaml_ty)
        {
            for (k, v) in yaml_obj {
                if !v.is_null() {
                    sdl_obj.entry(k.clone()).or_insert_with(|| v.clone());
                }
            }
        }
        out.push(sdl_ty);
    }
    for yaml_ty in existing {
        if !out.iter().any(|t| t.get("name") == yaml_ty.get("name")) {
            out.push(yaml_ty);
        }
    }
    ct.insert(key.into(), Value::Array(out));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_signatures_and_types() {
        let mut actions = vec![json!({
            "name": "foo",
            "definition": {"kind": "synchronous", "handler": "http://localhost:3000"}
        })];
        let mut custom_types = json!({
            "input_objects": [{"name": "fooInput"}],
            "objects": [{"name": "fooOutput", "relationships": [{"name": "r"}]}]
        });
        let sdl = r#"
            type Mutation { foo(arg1: fooInput!): fooOutput }
            input fooInput { username: String! password: String! }
            type fooOutput { accessToken: String! }
        "#;
        merge(&mut actions, &mut custom_types, sdl).unwrap();
        assert_eq!(
            actions[0]["definition"],
            json!({
                "kind": "synchronous",
                "handler": "http://localhost:3000",
                "type": "mutation",
                "arguments": [{"name": "arg1", "type": "fooInput!"}],
                "output_type": "fooOutput"
            })
        );
        assert_eq!(
            custom_types["objects"][0],
            json!({
                "name": "fooOutput",
                "fields": [{"name": "accessToken", "type": "String!"}],
                "relationships": [{"name": "r"}]
            })
        );
        assert_eq!(
            custom_types["input_objects"][0]["fields"][1],
            json!({"name": "password", "type": "String!"})
        );
    }
}
