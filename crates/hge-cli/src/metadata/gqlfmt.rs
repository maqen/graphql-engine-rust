//! GraphQL formatting as gqlparser's `formatter` does it (`WithIndent("  ")`), used by
//! the official CLI for remote schema permissions and query collection queries.

use async_graphql_parser::types::{
    BaseType, ConstDirective, DirectiveDefinition, DocumentOperations, ExecutableDocument,
    FieldDefinition, InputValueDefinition, OperationType, Selection, SelectionSet, ServiceDocument,
    Type, TypeDefinition, TypeKind, TypeSystemDefinition, VariableDefinition,
};
use async_graphql_parser::{Pos, Positioned};
use async_graphql_value::{ConstValue, Value};

use crate::gofmt::quote;

struct Fmt<'s> {
    out: String,
    indent: usize,
    pad_next: bool,
    line_head: bool,
    /// The source, to recover number literals as written.
    src: &'s str,
}

impl<'s> Fmt<'s> {
    fn new(src: &'s str) -> Self {
        Fmt {
            out: String::new(),
            indent: 0,
            pad_next: false,
            line_head: false,
            src,
        }
    }

    fn write_indent(&mut self) {
        if self.line_head {
            for _ in 0..self.indent {
                self.out.push_str("  ");
            }
        }
        self.line_head = false;
        self.pad_next = false;
    }

    fn newline(&mut self) -> &mut Self {
        self.out.push('\n');
        self.line_head = true;
        self.pad_next = false;
        self
    }

    fn word(&mut self, w: &str) -> &mut Self {
        if self.line_head {
            self.write_indent();
        }
        if self.pad_next {
            self.out.push(' ');
        }
        self.out.push_str(w.trim());
        self.pad_next = true;
        self
    }

    fn string(&mut self, s: &str) -> &mut Self {
        if self.line_head {
            self.write_indent();
        }
        if self.pad_next {
            self.out.push(' ');
        }
        self.out.push_str(s);
        self.pad_next = false;
        self
    }

    fn no_padding(&mut self) -> &mut Self {
        self.pad_next = false;
        self
    }

    fn need_padding(&mut self) -> &mut Self {
        self.pad_next = true;
        self
    }

    fn description(&mut self, d: Option<&Positioned<String>>) {
        let Some(d) = d else { return };
        if d.node.is_empty() {
            return;
        }
        self.string("\"\"\"");
        self.newline();
        for line in d.node.split('\n') {
            self.string(line).newline();
        }
        self.string("\"\"\"").newline();
    }

    // --- values

    fn number_at(&self, pos: Pos) -> Option<String> {
        let line = self.src.split('\n').nth(pos.line.checked_sub(1)?)?;
        let start: usize = line.char_indices().nth(pos.column.checked_sub(1)?)?.0;
        let text: String = line[start..]
            .chars()
            .take_while(|c| c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E'))
            .collect();
        (!text.is_empty()).then_some(text)
    }

    fn value_string(&self, v: &Positioned<Value>) -> String {
        self.value_inner(&v.node, Some(v.pos))
    }

    fn value_inner(&self, v: &Value, pos: Option<Pos>) -> String {
        match v {
            Value::Variable(n) => format!("${n}"),
            Value::Number(n) => pos
                .and_then(|p| self.number_at(p))
                .unwrap_or_else(|| n.to_string()),
            Value::Null => "null".into(),
            Value::Boolean(b) => b.to_string(),
            Value::Enum(e) => e.to_string(),
            Value::String(s) => quote(s),
            Value::Binary(_) => "\"\"".into(),
            Value::List(items) => {
                let parts: Vec<String> = items.iter().map(|i| self.value_inner(i, None)).collect();
                format!("[{}]", parts.join(","))
            }
            Value::Object(fields) => {
                let parts: Vec<String> = fields
                    .iter()
                    .map(|(k, v)| format!("{k}:{}", self.value_inner(v, None)))
                    .collect();
                format!("{{{}}}", parts.join(","))
            }
        }
    }

    fn const_value_string(&self, v: &Positioned<ConstValue>) -> String {
        match &v.node {
            ConstValue::Number(n) => self.number_at(v.pos).unwrap_or_else(|| n.to_string()),
            other => self.value_inner(&other.clone().into_value(), None),
        }
    }

    // --- shared pieces

    fn ty(&mut self, t: &Type) {
        self.word(&type_string(t));
    }

    fn directives(&mut self, dirs: &[Positioned<async_graphql_parser::types::Directive>]) {
        for d in dirs {
            self.string("@").word(&d.node.name.node);
            if !d.node.arguments.is_empty() {
                self.no_padding().string("(");
                let n = d.node.arguments.len();
                for (i, (name, value)) in d.node.arguments.iter().enumerate() {
                    let v = self.value_string(value);
                    self.word(&name.node)
                        .no_padding()
                        .string(":")
                        .need_padding();
                    self.string(&v);
                    if i != n - 1 {
                        self.no_padding().word(",");
                    }
                }
                self.string(")").need_padding();
            }
        }
    }

    fn const_directives(&mut self, dirs: &[Positioned<ConstDirective>]) {
        for d in dirs {
            self.string("@").word(&d.node.name.node);
            if !d.node.arguments.is_empty() {
                self.no_padding().string("(");
                let n = d.node.arguments.len();
                for (i, (name, value)) in d.node.arguments.iter().enumerate() {
                    let v = self.const_value_string(value);
                    self.word(&name.node)
                        .no_padding()
                        .string(":")
                        .need_padding();
                    self.string(&v);
                    if i != n - 1 {
                        self.no_padding().word(",");
                    }
                }
                self.string(")").need_padding();
            }
        }
    }

    // --- query documents

    fn query_document(&mut self, doc: &ExecutableDocument) {
        let mut ops: Vec<(
            Option<String>,
            &Positioned<async_graphql_parser::types::OperationDefinition>,
        )> = match &doc.operations {
            DocumentOperations::Single(op) => vec![(None, op)],
            DocumentOperations::Multiple(map) => {
                map.iter().map(|(k, v)| (Some(k.to_string()), v)).collect()
            }
        };
        ops.sort_by_key(|(_, op)| (op.pos.line, op.pos.column));
        for (name, op) in ops {
            let kind = match op.node.ty {
                OperationType::Query => "query",
                OperationType::Mutation => "mutation",
                OperationType::Subscription => "subscription",
            };
            self.word(kind);
            if let Some(name) = &name {
                self.word(name);
            }
            self.variable_definitions(&op.node.variable_definitions);
            self.directives(&op.node.directives);
            if !op.node.selection_set.node.items.is_empty() {
                self.selection_set(&op.node.selection_set.node);
                self.newline();
            }
        }
        let mut frags: Vec<_> = doc.fragments.iter().collect();
        frags.sort_by_key(|(_, f)| (f.pos.line, f.pos.column));
        for (name, f) in frags {
            self.word("fragment").word(name);
            self.word("on").word(&f.node.type_condition.node.on.node);
            self.directives(&f.node.directives);
            if !f.node.selection_set.node.items.is_empty() {
                self.selection_set(&f.node.selection_set.node);
                self.newline();
            }
        }
    }

    fn variable_definitions(&mut self, defs: &[Positioned<VariableDefinition>]) {
        if defs.is_empty() {
            return;
        }
        self.string("(");
        let n = defs.len();
        for (i, d) in defs.iter().enumerate() {
            self.string("$")
                .word(&d.node.name.node)
                .no_padding()
                .string(":")
                .need_padding();
            self.ty(&d.node.var_type.node);
            if let Some(default) = &d.node.default_value {
                let v = self.const_value_string(default);
                self.word("=");
                self.string(&v);
            }
            if i != n - 1 {
                self.no_padding().word(",");
            }
        }
        self.no_padding().string(")").need_padding();
    }

    fn selection_set(&mut self, set: &SelectionSet) {
        if set.items.is_empty() {
            return;
        }
        self.string("{").newline();
        self.indent += 1;
        for sel in &set.items {
            match &sel.node {
                Selection::Field(f) => {
                    let f = &f.node;
                    if let Some(alias) = &f.alias
                        && alias.node != f.name.node
                    {
                        self.word(&alias.node)
                            .no_padding()
                            .string(":")
                            .need_padding();
                    }
                    self.word(&f.name.node);
                    if !f.arguments.is_empty() {
                        self.no_padding();
                        self.no_padding().string("(");
                        let n = f.arguments.len();
                        for (i, (name, value)) in f.arguments.iter().enumerate() {
                            let v = self.value_string(value);
                            self.word(&name.node)
                                .no_padding()
                                .string(":")
                                .need_padding();
                            self.string(&v);
                            if i != n - 1 {
                                self.no_padding().word(",");
                            }
                        }
                        self.string(")").need_padding();
                        self.need_padding();
                    }
                    self.directives(&f.directives);
                    self.selection_set(&f.selection_set.node);
                }
                Selection::FragmentSpread(s) => {
                    self.word("...");
                    self.word(&s.node.fragment_name.node);
                    self.directives(&s.node.directives);
                }
                Selection::InlineFragment(i) => {
                    self.word("...");
                    if let Some(tc) = &i.node.type_condition {
                        self.word("on").word(&tc.node.on.node);
                    }
                    self.directives(&i.node.directives);
                    self.selection_set(&i.node.selection_set.node);
                }
            }
            self.newline();
        }
        self.indent -= 1;
        self.string("}");
    }

    // --- schema documents

    fn schema_document(&mut self, doc: &ServiceDocument) {
        let mut schemas = Vec::new();
        let mut schema_exts = Vec::new();
        let mut directives = Vec::new();
        let mut types = Vec::new();
        let mut type_exts = Vec::new();
        for def in &doc.definitions {
            match def {
                TypeSystemDefinition::Schema(s) if s.node.extend => schema_exts.push(s),
                TypeSystemDefinition::Schema(s) => schemas.push(s),
                TypeSystemDefinition::Directive(d) => directives.push(d),
                TypeSystemDefinition::Type(t) if t.node.extend => type_exts.push(t),
                TypeSystemDefinition::Type(t) => types.push(t),
            }
        }
        self.schema_definitions(&schemas, false);
        self.schema_definitions(&schema_exts, true);
        for d in directives {
            self.directive_definition(&d.node);
        }
        for t in types {
            self.type_definition(&t.node, false);
        }
        for t in type_exts {
            self.type_definition(&t.node, true);
        }
    }

    fn schema_definitions(
        &mut self,
        defs: &[&Positioned<async_graphql_parser::types::SchemaDefinition>],
        extension: bool,
    ) {
        if defs.is_empty() {
            return;
        }
        if extension {
            self.word("extend");
        }
        self.word("schema");
        self.indent += 1;
        for d in defs {
            self.const_directives(&d.node.directives);
        }
        self.indent -= 1;
        let empty = defs.iter().all(|d| {
            d.node.query.is_none() && d.node.mutation.is_none() && d.node.subscription.is_none()
        });
        if !extension || !empty {
            self.string("{").newline();
            self.indent += 1;
            for d in defs {
                for (op, ty) in [
                    ("query", &d.node.query),
                    ("mutation", &d.node.mutation),
                    ("subscription", &d.node.subscription),
                ] {
                    if let Some(ty) = ty {
                        self.word(op).no_padding().string(":").need_padding();
                        self.word(&ty.node);
                        self.newline();
                    }
                }
            }
            self.indent -= 1;
            self.string("}");
        }
        self.newline();
    }

    fn directive_definition(&mut self, d: &DirectiveDefinition) {
        self.description(d.description.as_ref());
        self.word("directive").string("@").word(&d.name.node);
        if !d.arguments.is_empty() {
            self.no_padding();
            self.argument_definitions(&d.arguments);
        }
        if d.is_repeatable {
            self.word("repeatable");
        }
        if !d.locations.is_empty() {
            self.word("on");
            let n = d.locations.len();
            for (i, loc) in d.locations.iter().enumerate() {
                self.word(&directive_location(&loc.node));
                if i != n - 1 {
                    self.word("|");
                }
            }
        }
        self.newline();
    }

    fn argument_definitions(&mut self, args: &[Positioned<InputValueDefinition>]) {
        if args.is_empty() {
            return;
        }
        self.string("(");
        let n = args.len();
        for (i, a) in args.iter().enumerate() {
            let a = &a.node;
            let has_desc = a.description.as_ref().is_some_and(|d| !d.node.is_empty());
            if has_desc {
                self.newline();
                self.indent += 1;
                self.description(a.description.as_ref());
            }
            self.word(&a.name.node)
                .no_padding()
                .string(":")
                .need_padding();
            self.ty(&a.ty.node);
            if let Some(default) = &a.default_value {
                let v = self.const_value_string(default);
                self.word("=");
                self.string(&v);
            }
            self.need_padding();
            self.const_directives(&a.directives);
            if has_desc {
                self.indent -= 1;
                self.newline();
            }
            if i != n - 1 && !has_desc {
                self.no_padding().word(",");
            }
        }
        self.no_padding().string(")").need_padding();
    }

    fn field_definition(&mut self, f: &FieldDefinition) {
        if f.name.node.starts_with("__") {
            return;
        }
        self.description(f.description.as_ref());
        self.word(&f.name.node).no_padding();
        self.argument_definitions(&f.arguments);
        self.no_padding().string(":").need_padding();
        self.ty(&f.ty.node);
        self.const_directives(&f.directives);
        self.newline();
    }

    fn input_field_definition(&mut self, f: &InputValueDefinition) {
        if f.name.node.starts_with("__") {
            return;
        }
        self.description(f.description.as_ref());
        self.word(&f.name.node).no_padding();
        self.no_padding().string(":").need_padding();
        self.ty(&f.ty.node);
        if let Some(default) = &f.default_value {
            let v = self.const_value_string(default);
            self.word("=");
            self.string(&v);
        }
        self.const_directives(&f.directives);
        self.newline();
    }

    fn type_definition(&mut self, t: &TypeDefinition, extend: bool) {
        if t.name.node.starts_with("__") {
            return;
        }
        self.description(t.description.as_ref());
        if extend {
            self.word("extend");
        }
        let (kw, implements): (&str, &[Positioned<async_graphql_value::Name>]) = match &t.kind {
            TypeKind::Scalar => ("scalar", &[]),
            TypeKind::Object(o) => ("type", &o.implements),
            TypeKind::Interface(i) => ("interface", &i.implements),
            TypeKind::Union(_) => ("union", &[]),
            TypeKind::Enum(_) => ("enum", &[]),
            TypeKind::InputObject(_) => ("input", &[]),
        };
        self.word(kw).word(&t.name.node);
        if !implements.is_empty() {
            let names: Vec<&str> = implements.iter().map(|n| n.node.as_str()).collect();
            self.word("implements").word(&names.join(" & "));
        }
        self.const_directives(&t.directives);
        if let TypeKind::Union(u) = &t.kind
            && !u.members.is_empty()
        {
            let names: Vec<&str> = u.members.iter().map(|n| n.node.as_str()).collect();
            self.word("=").word(&names.join(" | "));
        }
        match &t.kind {
            TypeKind::Object(o) => self.fields(&o.fields),
            TypeKind::Interface(i) => self.fields(&i.fields),
            TypeKind::InputObject(i) if !i.fields.is_empty() => {
                self.string("{").newline();
                self.indent += 1;
                for f in &i.fields {
                    self.input_field_definition(&f.node);
                }
                self.indent -= 1;
                self.string("}");
            }
            TypeKind::Enum(e) if !e.values.is_empty() => {
                self.string("{").newline();
                self.indent += 1;
                for v in &e.values {
                    self.description(v.node.description.as_ref());
                    self.word(&v.node.value.node);
                    self.const_directives(&v.node.directives);
                    self.newline();
                }
                self.indent -= 1;
                self.string("}");
            }
            _ => {}
        }
        self.newline();
    }

    fn fields(&mut self, fields: &[Positioned<FieldDefinition>]) {
        if fields.is_empty() {
            return;
        }
        self.string("{").newline();
        self.indent += 1;
        for f in fields {
            self.field_definition(&f.node);
        }
        self.indent -= 1;
        self.string("}");
    }
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

fn directive_location(loc: &async_graphql_parser::types::DirectiveLocation) -> String {
    let debug = format!("{loc:?}");
    let mut out = String::new();
    for (i, c) in debug.chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(c.to_ascii_uppercase());
    }
    out
}

/// `FormatQueryDocument`, or `None` when the query does not parse.
pub fn format_query(src: &str) -> Option<String> {
    let doc = async_graphql_parser::parse_query(src).ok()?;
    let mut f = Fmt::new(src);
    f.query_document(&doc);
    Some(f.out)
}

/// `FormatSchemaDocument`, or `None` when the schema does not parse.
pub fn format_schema(src: &str) -> Option<String> {
    let doc = async_graphql_parser::parse_schema(src).ok()?;
    let mut f = Fmt::new(src);
    f.schema_document(&doc);
    Some(f.out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn formats_queries_like_gqlparser() {
        let q = "query artist_by_id($id: Int!) { artist_by_pk(artist_id: $id) { name albums(limit: 2, order_by: {title: asc}) { title } } }";
        assert_eq!(
            super::format_query(q).unwrap(),
            "query artist_by_id ($id: Int!) {\n  artist_by_pk(artist_id: $id) {\n    name\n    albums(limit: 2, order_by: {title:asc}) {\n      title\n    }\n  }\n}\n"
        );
    }

    #[test]
    fn formats_schemas_like_gqlparser() {
        let s = "schema { query: Query } type Query { artist(id: Int!, \"the name\" name: String): Artist } type Artist { id: Int! name: String }";
        assert_eq!(
            super::format_schema(s).unwrap(),
            "schema {\n  query: Query\n}\ntype Query {\n  artist(id: Int!,\n    \"\"\"\n    the name\n    \"\"\"\n    name: String\n  ): Artist\n}\ntype Artist {\n  id: Int!\n  name: String\n}\n"
        );
    }
}
