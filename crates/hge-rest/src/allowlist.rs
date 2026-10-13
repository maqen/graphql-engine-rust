//! The allowlist (spec 13 §2): which GraphQL documents non-admin roles may run when
//! `HASURA_GRAPHQL_ENABLE_ALLOWLIST` is set.
//!
//! V2 compares the request's parsed document with each allowlisted query by AST equality
//! after removing `__typename` fields everywhere (`NormalizedQuery`). Its AST keeps field
//! and directive arguments and object literal fields in hash maps, so their order does not
//! matter; everything else (selection order, aliases, variable definitions, literals,
//! shorthand vs `query {…}`) does. We compare a canonical text rendering with those same
//! rules. One deliberate difference: the parser we use does not keep the order of
//! definitions, so the same operations and fragments in another order also match.

use std::collections::HashSet;
use std::fmt::Write;

use async_graphql_parser::Positioned;
use async_graphql_parser::types::{
    BaseType, Directive, DocumentOperations, ExecutableDocument, OperationDefinition, Selection,
    SelectionSet, Type,
};
use async_graphql_value::{ConstValue, Value};
use hge_core::Error;
use hge_metadata::Metadata;
use serde_json::json;

/// The canonical forms of every globally allowlisted query.
#[derive(Debug, Default)]
pub struct Allowlist {
    queries: HashSet<String>,
}

impl Allowlist {
    /// CE uses only the collections with `scope.global = true` (`AllowlistModeGlobalOnly`);
    /// role-scoped entries are ignored.
    pub fn build(metadata: &Metadata) -> Self {
        Allowlist {
            queries: global_queries(metadata),
        }
    }

    pub fn allows(&self, doc: &ExecutableDocument, text: &str) -> bool {
        self.queries.contains(&canonical(doc, text))
    }

    /// `Ok` when `doc` (parsed from `text`) is allowlisted, else V2's error.
    pub fn check(&self, doc: &ExecutableDocument, text: &str) -> Result<(), Error> {
        if self.allows(doc, text) {
            Ok(())
        } else {
            Err(not_allowed())
        }
    }
}

/// The canonical forms of the queries in globally allowlisted collections.
pub(crate) fn global_queries(metadata: &Metadata) -> HashSet<String> {
    let mut queries = HashSet::new();
    for entry in metadata.allowlist.iter().filter(|e| e.scope.is_global()) {
        let Some(collection) = metadata
            .query_collections
            .iter()
            .find(|c| c.name == entry.collection)
        else {
            continue;
        };
        for q in &collection.definition.queries {
            // Documents the request parser rejects can never match a request.
            if let Ok(doc) = async_graphql_parser::parse_query(&q.query) {
                queries.insert(canonical(&doc, &q.query));
            }
        }
    }
    queries
}

/// V2's error for a query outside the allowlist.
pub fn not_allowed() -> Error {
    Error::validation("query is not allowed")
        .with_internal(json!({"message": "query is not in any of the allowlists"}))
}

/// A rendering of `doc` that is equal for two documents exactly when V2's normalized ASTs
/// are (up to definition order, see the module docs). `text` is the source of `doc`, used
/// to tell the shorthand `{ … }` form from `query { … }`.
pub fn canonical(doc: &ExecutableDocument, text: &str) -> String {
    // The parser treats a lone `\r` as a line break but does not count it as a new line in
    // positions, which would make `starts_with_brace` look at the wrong character. Such
    // documents are canonicalized from a copy with `\n` line breaks instead (a raw `\r`
    // cannot occur inside a string literal, and block strings normalize line breaks).
    if has_lone_cr(text) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        if let Ok(doc) = async_graphql_parser::parse_query(&normalized) {
            return canonical_lf(&doc, &normalized);
        }
    }
    canonical_lf(doc, text)
}

fn has_lone_cr(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes
        .iter()
        .enumerate()
        .any(|(i, &b)| b == b'\r' && bytes.get(i + 1) != Some(&b'\n'))
}

fn canonical_lf(doc: &ExecutableDocument, text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut ops: Vec<(Option<&str>, &Positioned<OperationDefinition>)> = match &doc.operations {
        DocumentOperations::Single(op) => vec![(None, op)],
        DocumentOperations::Multiple(ops) => {
            ops.iter().map(|(n, op)| (Some(n.as_str()), op)).collect()
        }
    };
    ops.sort_by_key(|(name, _)| *name);
    for (name, op) in ops {
        if name.is_none() && starts_with_brace(text, op.pos) {
            out.push('{');
        } else {
            let _ = write!(out, "{} {}", op.node.ty, name.unwrap_or(""));
            out.push('(');
            for v in &op.node.variable_definitions {
                let v = &v.node;
                let _ = write!(out, "${}:", v.name.node);
                write_type(&mut out, &v.var_type.node);
                if let Some(d) = &v.default_value {
                    out.push('=');
                    write_const(&mut out, &d.node);
                }
                write_directives(&mut out, &v.directives);
                out.push(',');
            }
            out.push(')');
            write_directives(&mut out, &op.node.directives);
            out.push('{');
        }
        write_selection_set(&mut out, &op.node.selection_set.node);
        out.push_str("}\n");
    }
    let mut fragments: Vec<_> = doc.fragments.iter().collect();
    fragments.sort_by_key(|(name, _)| name.as_str());
    for (name, f) in fragments {
        let _ = write!(
            out,
            "fragment {name} on {}",
            f.node.type_condition.node.on.node
        );
        write_directives(&mut out, &f.node.directives);
        out.push('{');
        write_selection_set(&mut out, &f.node.selection_set.node);
        out.push_str("}\n");
    }
    out
}

/// Whether the source character at `pos` (1-based line and character column) is `{`.
fn starts_with_brace(text: &str, pos: async_graphql_parser::Pos) -> bool {
    text.split('\n')
        .nth(pos.line.saturating_sub(1))
        .and_then(|line| line.chars().nth(pos.column.saturating_sub(1)))
        == Some('{')
}

fn write_selection_set(out: &mut String, set: &SelectionSet) {
    for item in &set.items {
        match &item.node {
            Selection::Field(f) => {
                let f = &f.node;
                if f.name.node == "__typename" {
                    continue;
                }
                if let Some(alias) = &f.alias {
                    let _ = write!(out, "{}:", alias.node);
                }
                out.push_str(&f.name.node);
                write_arguments(
                    out,
                    f.arguments.iter().map(|(n, v)| (n.node.as_str(), &v.node)),
                );
                write_directives(out, &f.directives);
                // V2 drops `__typename` first, so `a { __typename }` is `a`.
                let has_selection = f.selection_set.node.items.iter().any(
                    |s| !matches!(&s.node, Selection::Field(f) if f.node.name.node == "__typename"),
                );
                if has_selection {
                    out.push('{');
                    write_selection_set(out, &f.selection_set.node);
                    out.push('}');
                }
            }
            Selection::FragmentSpread(s) => {
                let _ = write!(out, "...{}", s.node.fragment_name.node);
                write_directives(out, &s.node.directives);
            }
            Selection::InlineFragment(i) => {
                out.push_str("...");
                if let Some(t) = &i.node.type_condition {
                    let _ = write!(out, " on {}", t.node.on.node);
                }
                write_directives(out, &i.node.directives);
                out.push('{');
                write_selection_set(out, &i.node.selection_set.node);
                out.push('}');
            }
        }
        out.push(' ');
    }
}

fn write_directives(out: &mut String, directives: &[Positioned<Directive>]) {
    for d in directives {
        let _ = write!(out, "@{}", d.node.name.node);
        write_arguments(
            out,
            d.node
                .arguments
                .iter()
                .map(|(n, v)| (n.node.as_str(), &v.node)),
        );
    }
}

/// Arguments sorted by name (V2 keeps them in a hash map).
fn write_arguments<'a>(out: &mut String, args: impl Iterator<Item = (&'a str, &'a Value)>) {
    // A repeated argument keeps its last value, like V2's hash map.
    let mut by_name: std::collections::BTreeMap<&str, &Value> = Default::default();
    for (name, value) in args {
        by_name.insert(name, value);
    }
    if by_name.is_empty() {
        return;
    }
    let args = by_name;
    out.push('(');
    for (name, value) in args {
        let _ = write!(out, "{name}:");
        write_value(out, value);
        out.push(',');
    }
    out.push(')');
}

fn write_type(out: &mut String, ty: &Type) {
    match &ty.base {
        BaseType::Named(name) => out.push_str(name),
        BaseType::List(inner) => {
            out.push('[');
            write_type(out, inner);
            out.push(']');
        }
    }
    if !ty.nullable {
        out.push('!');
    }
}

fn write_value(out: &mut String, value: &Value) {
    match value {
        Value::Variable(name) => {
            let _ = write!(out, "${name}");
        }
        Value::List(items) => {
            out.push('[');
            for item in items {
                write_value(out, item);
                out.push(',');
            }
            out.push(']');
        }
        Value::Object(fields) => {
            let mut fields: Vec<_> = fields.iter().collect();
            fields.sort_by_key(|(name, _)| name.as_str());
            out.push('{');
            for (name, v) in fields {
                let _ = write!(out, "{name}:");
                write_value(out, v);
                out.push(',');
            }
            out.push('}');
        }
        other => write_scalar(out, other.clone().into_const()),
    }
}

fn write_const(out: &mut String, value: &ConstValue) {
    write_value(out, &value.clone().into_value());
}

fn write_scalar(out: &mut String, value: Option<ConstValue>) {
    match value {
        // Strings are quoted (so `"E"` and the enum `E` differ); numbers keep their
        // literal form, so `1` and `1.0` differ like V2's Int and Float literals.
        Some(ConstValue::String(s)) => {
            let _ = write!(out, "{}", serde_json::Value::String(s));
        }
        Some(ConstValue::Enum(e)) => out.push_str(&e),
        Some(ConstValue::Number(n)) => {
            let _ = write!(out, "{n}");
        }
        Some(ConstValue::Boolean(b)) => {
            let _ = write!(out, "{b}");
        }
        Some(ConstValue::Null) | None => out.push_str("null"),
        Some(other) => {
            let _ = write!(out, "{other}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn same(a: &str, b: &str) -> bool {
        let pa = async_graphql_parser::parse_query(a).unwrap();
        let pb = async_graphql_parser::parse_query(b).unwrap();
        canonical(&pa, a) == canonical(&pb, b)
    }

    #[test]
    fn normalization_follows_v2() {
        // Ignored: whitespace, commas, comments, argument and object-field order, __typename.
        assert!(same(
            "query q { a(x: 1, y: {p: 1, q: \"s\"}) { b } }",
            "# hi\nquery q{a(y:{q:\"s\",p:1},x:1){b,__typename} __typename}"
        ));
        assert!(same(
            "query { u { ...F } } fragment F on u { id __typename }",
            "query { u { ...F } } fragment F on u { id }"
        ));
        assert!(same(r#"{ a(s: """x""") }"#, r#"{ a(s: "x") }"#));
        // Significant: shorthand vs typed, names, aliases, order, literals, variables.
        assert!(!same("{ a }", "query { a }"));
        assert!(!same("query q { a }", "query r { a }"));
        assert!(!same("{ a b }", "{ b a }"));
        assert!(!same("{ x: a }", "{ a }"));
        assert!(!same("{ a(x: 1) }", "{ a(x: 1.0) }"));
        assert!(!same("{ a(x: E) }", "{ a(x: \"E\") }"));
        assert!(!same(
            "query ($a: Int) { a(x: $a) }",
            "query ($a: Int!) { a(x: $a) }"
        ));
        assert!(!same("{ a @include(if: true) }", "{ a }"));
        assert!(!same(
            "query { u { ...F } } fragment F on u { id }",
            "query { u { id } }"
        ));
        assert!(same("{ a { __typename } }", "{ a }"));
        assert!(same("{ a(x: 1, x: 2) }", "{ a(x: 2) }"));
        // A lone CR must not confuse the shorthand check (it moves no line in `Pos`).
        assert!(!same(
            "#{\r subscription { users { id } }",
            "{ users { id } }"
        ));
        assert!(!same("#c\r{ a }", "query { a }"));
        assert!(same("#c\r{ a }", "{ a }"));
        assert!(same("\u{feff}\r\n{ a }", "{ a }"));
    }
}
