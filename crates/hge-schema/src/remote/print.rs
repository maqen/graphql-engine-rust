//! The GraphQL printer V2 uses for requests to remote servers (spec 08 §6.6). Remote
//! servers report error locations against this text, so the spacing is reproduced exactly:
//!
//! ```text
//! query  { f0: message(id: 1) { msg  errorMsg  } }
//! ```

use std::fmt::Write;

use async_graphql_value::{ConstValue, Value as GqlValue};

use crate::model::TypeRef;

/// A selection in an outgoing request.
#[derive(Debug, Clone, PartialEq)]
pub enum OutSelection {
    Field(OutField),
    InlineFragment {
        on: String,
        selection: Vec<OutSelection>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutField {
    pub alias: Option<String>,
    pub name: String,
    pub args: Vec<(String, GqlValue)>,
    pub selection: Vec<OutSelection>,
}

impl OutField {
    pub fn new(name: impl Into<String>) -> Self {
        OutField {
            alias: None,
            name: name.into(),
            args: vec![],
            selection: vec![],
        }
    }

    /// Set the alias unless it is the field name (V2 drops `x: x`).
    pub fn with_alias(mut self, alias: &str) -> Self {
        if alias != self.name {
            self.alias = Some(alias.to_owned());
        }
        self
    }
}

/// An outgoing operation.
#[derive(Debug, Clone)]
pub struct OutOperation<'a> {
    /// `query` or `mutation`.
    pub kind: &'a str,
    pub name: Option<&'a str>,
    pub variables: Vec<(String, TypeRef)>,
    pub selection: Vec<OutSelection>,
}

pub fn operation(op: &OutOperation<'_>) -> String {
    let mut out = String::with_capacity(128);
    out.push_str(op.kind);
    out.push(' ');
    if let Some(name) = op.name {
        out.push_str(name);
    }
    if !op.variables.is_empty() {
        out.push('(');
        for (i, (name, ty)) in op.variables.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write!(out, "${name}: {ty}").expect("write to string");
        }
        out.push(')');
    }
    out.push(' ');
    selection_set(&mut out, &op.selection);
    out
}

fn selection_set(out: &mut String, selection: &[OutSelection]) {
    if selection.is_empty() {
        return;
    }
    out.push_str("{ ");
    for (i, sel) in selection.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        match sel {
            OutSelection::Field(f) => field(out, f),
            OutSelection::InlineFragment { on, selection } => {
                out.push_str("... on ");
                out.push_str(on);
                selection_set(out, selection);
            }
        }
    }
    out.push_str(" }");
}

fn field(out: &mut String, f: &OutField) {
    if let Some(alias) = &f.alias {
        out.push_str(alias);
        out.push_str(": ");
    }
    out.push_str(&f.name);
    if !f.args.is_empty() {
        out.push('(');
        for (i, (name, v)) in f.args.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(name);
            out.push_str(": ");
            value(out, v);
        }
        out.push(')');
    }
    out.push(' ');
    selection_set(out, &f.selection);
}

pub fn value(out: &mut String, v: &GqlValue) {
    match v {
        GqlValue::Variable(n) => {
            out.push('$');
            out.push_str(n);
        }
        GqlValue::Null => out.push_str("null"),
        GqlValue::Number(n) => write!(out, "{n}").expect("write to string"),
        GqlValue::String(s) => {
            out.push_str(&serde_json::to_string(s).expect("string serializes"));
        }
        GqlValue::Boolean(b) => out.push_str(if *b { "true" } else { "false" }),
        GqlValue::Binary(b) => {
            out.push_str(
                &serde_json::to_string(&String::from_utf8_lossy(b)).expect("string serializes"),
            );
        }
        GqlValue::Enum(e) => out.push_str(e),
        GqlValue::List(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                value(out, item);
            }
            out.push(']');
        }
        GqlValue::Object(fields) => {
            out.push('{');
            for (i, (k, item)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(k);
                out.push_str(": ");
                value(out, item);
            }
            out.push('}');
        }
    }
}

/// A constant value as GraphQL text (introspection `defaultValue`).
pub fn const_value(v: &ConstValue) -> String {
    let mut out = String::new();
    value(&mut out, &v.clone().into_value());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_graphql_value::Name;

    fn leaf(name: &str) -> OutSelection {
        OutSelection::Field(OutField::new(name))
    }

    #[test]
    fn join_query_matches_v2_spacing() {
        let sel = (1..=3)
            .map(|i| {
                OutSelection::Field(OutField {
                    alias: Some(format!("f{}", i - 1)),
                    name: "message".into(),
                    args: vec![("id".into(), GqlValue::Number(i.into()))],
                    selection: vec![leaf("msg"), leaf("errorMsg")],
                })
            })
            .collect();
        let text = operation(&OutOperation {
            kind: "query",
            name: None,
            variables: vec![],
            selection: sel,
        });
        assert_eq!(
            text,
            "query  { f0: message(id: 1) { msg  errorMsg  } f1: message(id: 2) { msg  errorMsg  } f2: message(id: 3) { msg  errorMsg  } }"
        );
        // V2's golden test: `errorMsg` at columns 36, 74 and 112.
        let cols: Vec<usize> = text.match_indices("errorMsg").map(|(i, _)| i + 1).collect();
        assert_eq!(cols, vec![36, 74, 112]);
    }

    #[test]
    fn variables_objects_and_fragments() {
        let mut obj = indexmap::IndexMap::new();
        obj.insert(Name::new("low"), GqlValue::Number(0.into()));
        obj.insert(Name::new("high"), GqlValue::Variable(Name::new("v")));
        let text = operation(&OutOperation {
            kind: "query",
            name: Some("Q"),
            variables: vec![("v".into(), TypeRef::nn("Int"))],
            selection: vec![
                OutSelection::Field(OutField {
                    alias: None,
                    name: "getValues".into(),
                    args: vec![("range".into(), GqlValue::Object(obj))],
                    selection: vec![],
                }),
                OutSelection::Field(OutField {
                    alias: None,
                    name: "node".into(),
                    args: vec![("id".into(), GqlValue::String("x".into()))],
                    selection: vec![
                        leaf("__typename"),
                        OutSelection::InlineFragment {
                            on: "Film".into(),
                            selection: vec![leaf("title")],
                        },
                    ],
                }),
            ],
        });
        assert_eq!(
            text,
            "query Q($v: Int!) { getValues(range: {low: 0,high: $v})  node(id: \"x\") { __typename  ... on Film{ title  } } }"
        );
    }
}
