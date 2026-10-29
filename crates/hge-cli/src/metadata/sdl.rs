//! `actions.graphql` <-> action metadata, as the CLI's `cli-ext` (`sdl to` / `sdl from`)
//! converts them.

use async_graphql_parser::types::{BaseType, Type, TypeKind, TypeSystemDefinition};
use serde_json::{Value, json};

use crate::yaml::Node;

// --- metadata -> SDL ("sdl to")

/// The SDL `cli-ext sdl to` prints for the actions and custom types of the metadata.
pub fn metadata_to_sdl(actions: &[&Node], custom_types: Option<&Node>) -> String {
    let mut sdl = String::new();
    for a in actions {
        let def = a.get("definition");
        let op = match def.and_then(|d| d.get("type")).and_then(Node::as_str) {
            Some("query") => "Query",
            _ => "Mutation",
        };
        let name = text(a.get("name"));
        let args = def.and_then(|d| d.get("arguments"));
        let output = text(def.and_then(|d| d.get("output_type")));
        let field = format!("  {name}{}: {output}", arguments_sdl(args));
        sdl.push_str(&format!("type {op} {{\n{field}\n}}\n\n"));
    }
    if let Some(types) = custom_types {
        // Keys in the order `parseCustomTypes` sees them (alphabetical after YAMLToJSON).
        for kind in ["enums", "input_objects", "objects", "scalars"] {
            let Some(Node::Seq(defs)) = types.get(kind) else {
                continue;
            };
            for t in defs {
                sdl.push_str(&type_sdl(kind, t));
            }
        }
    }
    sdl
}

fn text(n: Option<&Node>) -> String {
    match n {
        Some(Node::Scalar(s)) => s.value.clone(),
        _ => "undefined".into(),
    }
}

/// `""" description """ ` when the description is a non-empty string.
fn description(n: Option<&Node>) -> String {
    match n {
        Some(Node::Scalar(s)) if !(s.value.is_empty() || (!s.is_str && s.value == "null")) => {
            if !s.is_str && s.value == "false" {
                return String::new();
            }
            format!("\"\"\" {} \"\"\" ", s.value)
        }
        _ => String::new(),
    }
}

fn arguments_sdl(args: Option<&Node>) -> String {
    let Some(Node::Seq(args)) = args else {
        return String::new();
    };
    if args.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = args
        .iter()
        .map(|a| {
            format!(
                "    {}{}: {}",
                description(a.get("description")),
                text(a.get("name")),
                text(a.get("type"))
            )
        })
        .collect();
    format!("(\n{}\n  )", lines.join("\n"))
}

fn fields_sdl(fields: Option<&Node>) -> String {
    let Some(Node::Seq(fields)) = fields else {
        return String::new();
    };
    fields
        .iter()
        .map(|f| {
            let args = match f.get("arguments") {
                Some(a @ Node::Seq(_)) => arguments_sdl(Some(a)),
                _ => String::new(),
            };
            format!(
                "  {}{}{args}: {}",
                description(f.get("description")),
                text(f.get("name")),
                text(f.get("type"))
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn type_sdl(kind: &str, t: &Node) -> String {
    let desc = description(t.get("description"));
    let name = text(t.get("name"));
    match kind {
        "scalars" => format!("{desc}scalar {name}\n\n"),
        "enums" => {
            let values: Vec<String> = match t.get("values") {
                Some(Node::Seq(vs)) => vs
                    .iter()
                    .map(|v| {
                        format!(
                            "  {}{}",
                            description(v.get("description")),
                            text(v.get("value"))
                        )
                    })
                    .collect(),
                _ => Vec::new(),
            };
            format!("{desc}enum {name} {{\n{}\n}}\n\n", values.join("\n"))
        }
        "input_objects" => format!(
            "{desc}input {name} {{\n{}\n}}\n\n",
            fields_sdl(t.get("fields"))
        ),
        _ => format!(
            "{desc}type {name} {{\n{}\n}}\n\n",
            fields_sdl(t.get("fields"))
        ),
    }
}

// --- SDL -> metadata ("sdl from")

/// Actions and custom types parsed from `actions.graphql`.
pub struct FromSdl {
    pub actions: Vec<Value>,
    pub enums: Vec<Value>,
    pub input_objects: Vec<Value>,
    pub objects: Vec<Value>,
    pub scalars: Vec<Value>,
}

fn type_string(t: &Type) -> String {
    let inner = match &t.base {
        BaseType::Named(n) => n.to_string(),
        BaseType::List(inner) => format!("[{}]", type_string(inner)),
    };
    if t.nullable {
        inner
    } else {
        format!("{inner}!")
    }
}

fn desc_value(d: Option<&async_graphql_parser::Positioned<String>>) -> Value {
    match d {
        Some(d) => Value::String(d.node.trim().to_string()),
        None => Value::Null,
    }
}

pub fn sdl_to_metadata(sdl: &str) -> Result<FromSdl, String> {
    let mut out = FromSdl {
        actions: vec![],
        enums: vec![],
        input_objects: vec![],
        objects: vec![],
        scalars: vec![],
    };
    if sdl.trim().is_empty() {
        return Ok(out);
    }
    let doc = async_graphql_parser::parse_schema(sdl).map_err(|e| e.to_string())?;
    for def in &doc.definitions {
        let ty = match def {
            TypeSystemDefinition::Type(t) => &t.node,
            TypeSystemDefinition::Schema(_) => {
                return Err("Cannot read properties of undefined (reading 'value')".into());
            }
            TypeSystemDefinition::Directive(_) => continue,
        };
        let name = ty.name.node.to_string();
        if name == "Query" || name == "Mutation" {
            let action_type = if name == "Query" { "query" } else { "mutation" };
            if let TypeKind::Object(o) = &ty.kind {
                for f in &o.fields {
                    let f = &f.node;
                    let args: Vec<Value> = f
                        .arguments
                        .iter()
                        .map(|a| {
                            json!({
                                "name": a.node.name.node.to_string(),
                                "type": type_string(&a.node.ty.node),
                                "description": desc_value(a.node.description.as_ref()),
                            })
                        })
                        .collect();
                    out.actions.push(json!({
                        "name": f.name.node.to_string(),
                        "definition": {
                            "type": action_type,
                            "arguments": args,
                            "output_type": type_string(&f.ty.node),
                        }
                    }));
                }
            }
            continue;
        }
        if ty.extend {
            return Err("Cannot read properties of undefined (reading 'name')".into());
        }
        let description = desc_value(ty.description.as_ref());
        match &ty.kind {
            TypeKind::Scalar => out
                .scalars
                .push(json!({"name": name, "description": description})),
            TypeKind::Enum(e) => {
                let values: Vec<Value> = e
                    .values
                    .iter()
                    .map(|v| {
                        json!({
                            "value": v.node.value.node.to_string(),
                            "description": desc_value(v.node.description.as_ref()),
                        })
                    })
                    .collect();
                out.enums
                    .push(json!({"name": name, "description": description, "values": values}));
            }
            TypeKind::InputObject(i) => {
                let fields: Vec<Value> = i
                    .fields
                    .iter()
                    .map(|f| {
                        json!({
                            "name": f.node.name.node.to_string(),
                            "type": type_string(&f.node.ty.node),
                            "description": desc_value(f.node.description.as_ref()),
                        })
                    })
                    .collect();
                out.input_objects
                    .push(json!({"name": name, "description": description, "fields": fields}));
            }
            TypeKind::Object(o) => {
                let fields: Vec<Value> = o
                    .fields
                    .iter()
                    .map(|f| {
                        json!({
                            "name": f.node.name.node.to_string(),
                            "type": type_string(&f.node.ty.node),
                            "description": desc_value(f.node.description.as_ref()),
                        })
                    })
                    .collect();
                out.objects
                    .push(json!({"name": name, "description": description, "fields": fields}));
            }
            TypeKind::Interface(_) => {}
            TypeKind::Union(_) => {
                return Err("Cannot read properties of undefined (reading 'name')".into());
            }
        }
    }
    Ok(out)
}
