//! Static validation of query collections against the admin schema (V2's
//! `getInconsistentQueryCollections` and `Hasura.GraphQL.Analyse.diagnoseGraphQLQuery`).
//!
//! V2 checks only a little: that each query has one operation, its fragments inline, every
//! selected field exists with a selection set exactly when it is an object, fields sharing a
//! response key have one type, and variable types are input types. Arguments, directives
//! and fragment type conditions are not checked, and neither are we.

use std::collections::{HashMap, HashSet};

use async_graphql_parser::Positioned;
use async_graphql_parser::types::{
    DocumentOperations, ExecutableDocument, FragmentDefinition, OperationDefinition, OperationType,
    Selection, SelectionSet,
};
use async_graphql_value::{ConstValue, Name};
use hge_core::InconsistentObject;
use hge_metadata::Metadata;
use hge_schema::model::{ObjectDef, Schema, TypeDef, TypeRef};
use indexmap::IndexMap;

use crate::RestEndpoints;
use crate::allowlist::{canonical, global_queries};

/// One inconsistency per collection query that does not validate, in metadata order.
pub fn collection_issues(
    metadata: &Metadata,
    schema: &Schema,
    endpoints: &RestEndpoints,
) -> Vec<InconsistentObject> {
    let mut allowlisted = None;
    let mut issues = vec![];
    for collection in &metadata.query_collections {
        for q in &collection.definition.queries {
            let Some(errors) = diagnose(schema, &q.query) else {
                continue;
            };
            let mut reason = format!(
                "In query collection \"{}\" the query \"{}\" is invalid with the following \
                 error(s): {}",
                collection.name,
                q.name,
                dquote_list(errors.iter().rev())
            );
            let users = endpoint_users(endpoints, &q.query);
            if !users.is_empty() {
                reason.push_str(". This query is being used in the following REST endpoint(s): ");
                reason.push_str(&dquote_list(users.iter().rev()));
            }
            let allowlisted = allowlisted.get_or_insert_with(|| global_queries(metadata));
            if in_allowlist(allowlisted, metadata, &q.query) {
                reason.push_str(". This query is in allowlist.");
            }
            issues.push(InconsistentObject::new(
                "query_collections",
                format!("query_collections {} in {}", q.name, collection.name),
                reason,
                serde_json::to_value(q).unwrap_or_default(),
            ));
        }
    }
    issues
}

fn dquote_list<'a>(items: impl Iterator<Item = &'a String>) -> String {
    items
        .map(|s| format!("\"{s}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Routable endpoints whose query has this exact text, once per method (as V2 lists them).
fn endpoint_users(endpoints: &RestEndpoints, query: &str) -> Vec<String> {
    let mut users: Vec<_> = endpoints.iter().filter(|e| e.query == query).collect();
    users.sort_by(|a, b| (&a.url, &a.name).cmp(&(&b.url, &b.name)));
    users
        .into_iter()
        .flat_map(|e| {
            let methods: HashSet<_> = e.methods.iter().collect();
            std::iter::repeat_n(e.name.clone(), methods.len())
        })
        .collect()
}

/// V2 compares the query as written with the allowlist's normalized (`__typename`-free)
/// queries, so a query selecting `__typename` is never "in allowlist".
fn in_allowlist(allowlisted: &HashSet<String>, metadata: &Metadata, query: &str) -> bool {
    match async_graphql_parser::parse_query(query) {
        Ok(doc) => !mentions_typename(&doc) && allowlisted.contains(&canonical(&doc, query)),
        // Documents our parser rejects (several or no operations) match at least themselves.
        Err(_) => !query.contains("__typename") && in_global_collection(metadata, query),
    }
}

fn in_global_collection(metadata: &Metadata, query: &str) -> bool {
    metadata
        .allowlist
        .iter()
        .filter(|e| e.scope.is_global())
        .filter_map(|e| {
            metadata
                .query_collections
                .iter()
                .find(|c| c.name == e.collection)
        })
        .any(|c| c.definition.queries.iter().any(|q| q.query == query))
}

fn mentions_typename(doc: &ExecutableDocument) -> bool {
    fn in_set(set: &SelectionSet) -> bool {
        set.items.iter().any(|s| match &s.node {
            Selection::Field(f) => {
                f.node.name.node == "__typename" || in_set(&f.node.selection_set.node)
            }
            // V2's normalization does not look inside inline fragments.
            Selection::InlineFragment(_) | Selection::FragmentSpread(_) => false,
        })
    }
    doc.operations
        .iter()
        .any(|(_, op)| in_set(&op.node.selection_set.node))
        || doc
            .fragments
            .values()
            .any(|f| in_set(&f.node.selection_set.node))
}

/// The errors of one query in the order V2 finds them (it reports them reversed), or `None`
/// when it is valid. Queries the parser rejects outright are left to metadata parsing.
pub fn diagnose(schema: &Schema, query: &str) -> Option<Vec<String>> {
    use async_graphql_parser::Error as ParseError;
    let doc = match async_graphql_parser::parse_query(query) {
        Ok(doc) => doc,
        Err(
            ParseError::MultipleOperations { .. }
            | ParseError::OperationDuplicated { .. }
            | ParseError::MissingOperation,
        ) => {
            return Some(vec![EXACTLY_ONE.to_owned()]);
        }
        Err(ParseError::FragmentDuplicated { fragment, .. }) => {
            return Some(vec![format!(
                "multiple definitions for fragment \"{fragment}\""
            )]);
        }
        Err(_) => return None,
    };
    let op = match &doc.operations {
        DocumentOperations::Single(op) => &op.node,
        DocumentOperations::Multiple(ops) if ops.len() == 1 => {
            &ops.values().next().expect("one operation").node
        }
        DocumentOperations::Multiple(_) => return Some(vec![EXACTLY_ONE.to_owned()]),
    };
    let set = &op.selection_set.node;
    if let Err(e) = check_fragments(&doc, set, &mut vec![], &mut HashSet::new()) {
        return Some(vec![e]);
    }
    let (_, errors) = analyze(schema, op, &doc.fragments);
    (!errors.is_empty()).then_some(errors)
}

/// What V2's analysis learns of an operation: its selected fields (failing ones left out)
/// and its variables whose type is an input type.
pub(crate) struct Structure {
    pub fields: Fields,
    pub variables: Vec<(String, VarInfo)>,
}

pub(crate) struct VarInfo {
    pub ty: TypeRef,
    pub default: Option<ConstValue>,
}

/// V2's `analyzeGraphQLQuery` over an operation whose fragments are known to inline. The
/// errors are in the order found.
pub(crate) fn analyze(
    schema: &Schema,
    op: &OperationDefinition,
    fragments: &Fragments,
) -> (Structure, Vec<String>) {
    let mut analysis = Analysis {
        schema,
        fragments,
        path: vec!["$".to_owned()],
        errors: vec![],
    };
    let root = match op.ty {
        OperationType::Query => "query_root",
        OperationType::Mutation => "mutation_root",
        OperationType::Subscription => "subscription_root",
    };
    let result = match schema.get(root) {
        None => Err(analysis.diagnosis(format!("type '{root}' not found in the schema"))),
        Some(TypeDef::Object(o)) => analysis.object_selection_set(o, &op.selection_set.node),
        Some(_) => Err(analysis.diagnosis("the root type is not an object".to_owned())),
    };
    let fields = result.unwrap_or_else(|e| {
        analysis.errors.push(e);
        Fields::new()
    });
    let mut variables = vec![];
    for v in &op.variable_definitions {
        let v = &v.node;
        let ty = TypeRef::from_ast(&v.var_type.node);
        let base = ty.base();
        let issue = match schema.get(base) {
            None => Some(format!("type '{base}' not found in the schema")),
            Some(TypeDef::Object(_)) => {
                Some(format!("object '{base}' cannot be used in an input type"))
            }
            Some(TypeDef::Union(_)) => {
                Some(format!("union '{base}' cannot be used in an input type"))
            }
            Some(TypeDef::Interface(_)) => Some(format!(
                "interface '{base}' cannot be used in an input type"
            )),
            Some(_) => None,
        };
        match issue {
            Some(issue) => {
                analysis.path.push(v.name.node.to_string());
                let e = analysis.diagnosis(issue);
                analysis.errors.push(e);
                analysis.path.pop();
            }
            None => variables.push((
                v.name.node.to_string(),
                VarInfo {
                    ty,
                    default: v.default_value.as_ref().map(|d| d.node.clone()),
                },
            )),
        }
    }
    (Structure { fields, variables }, analysis.errors)
}

const EXACTLY_ONE: &str =
    "exactly one operation has to be present in the document when operationName is not specified";

/// V2 inlines fragments before the analysis: every spread reached from the operation must
/// be defined and not part of a cycle. The first problem in document order is the error.
fn check_fragments<'a>(
    doc: &'a ExecutableDocument,
    set: &'a SelectionSet,
    stack: &mut Vec<&'a str>,
    done: &mut HashSet<&'a str>,
) -> Result<(), String> {
    for item in &set.items {
        match &item.node {
            Selection::Field(f) => check_fragments(doc, &f.node.selection_set.node, stack, done)?,
            Selection::InlineFragment(i) => {
                check_fragments(doc, &i.node.selection_set.node, stack, done)?
            }
            Selection::FragmentSpread(s) => {
                let name = s.node.fragment_name.node.as_str();
                if done.contains(name) {
                    continue;
                }
                if let Some(at) = stack.iter().position(|n| *n == name) {
                    return Err(format!(
                        "the fragment definition(s) {} form a cycle",
                        english_list(&stack[at..])
                    ));
                }
                let Some(fragment) = doc.fragments.get(&s.node.fragment_name.node) else {
                    return Err(format!("reference to undefined fragment \"{name}\""));
                };
                stack.push(name);
                check_fragments(doc, &fragment.node.selection_set.node, stack, done)?;
                stack.pop();
                done.insert(name);
            }
        }
    }
    Ok(())
}

/// `a`, `a and b`, `a, b, and c`
fn english_list(items: &[&str]) -> String {
    match items {
        [one] => (*one).to_owned(),
        [a, b] => format!("{a} and {b}"),
        [init @ .., last] => format!("{}, and {last}", init.join(", ")),
        [] => String::new(),
    }
}

/// The type of a selected field, by what the analysis needs to know of it.
pub(crate) enum FieldInfo {
    Leaf(TypeRef),
    Object(TypeRef, IndexMap<String, FieldInfo>),
}

impl FieldInfo {
    pub fn ty(&self) -> &TypeRef {
        match self {
            FieldInfo::Leaf(t) | FieldInfo::Object(t, _) => t,
        }
    }
}

pub(crate) type Fields = IndexMap<String, FieldInfo>;
type Fragments = HashMap<Name, Positioned<FragmentDefinition>>;

struct Analysis<'a> {
    schema: &'a Schema,
    fragments: &'a Fragments,
    /// `$` and the field names (not aliases) down to the current selection set.
    path: Vec<String>,
    errors: Vec<String>,
}

impl<'a> Analysis<'a> {
    fn diagnosis(&self, message: String) -> String {
        format!("{}: {message}", self.path.join("."))
    }

    /// The fields of a selection set. Failing fields are recorded and skipped; a type clash
    /// between fields with the same response key fails the whole set.
    fn object_selection_set(
        &mut self,
        object: &'a ObjectDef,
        set: &'a SelectionSet,
    ) -> Result<Fields, String> {
        let parts: Vec<Fields> = set
            .items
            .iter()
            .filter_map(|item| self.selection(object, &item.node))
            .collect();
        let mut fields = Fields::new();
        for part in parts {
            fields = self.union(fields, part)?;
        }
        Ok(fields)
    }

    fn selection(&mut self, object: &'a ObjectDef, selection: &'a Selection) -> Option<Fields> {
        match selection {
            // The fields of a fragment are not merged with each other, only collected.
            Selection::InlineFragment(i) => self.fragment(object, &i.node.selection_set.node),
            Selection::FragmentSpread(s) => {
                let fragment = self.fragments.get(&s.node.fragment_name.node)?;
                self.fragment(object, &fragment.node.selection_set.node)
            }
            Selection::Field(f) => {
                let f = &f.node;
                self.path.push(f.name.node.to_string());
                let result = self.field(object, f);
                self.path.pop();
                match result {
                    Ok(info) => {
                        let key = f.alias.as_ref().unwrap_or(&f.name).node.to_string();
                        info.map(|info| Fields::from([(key, info)]))
                    }
                    Err(e) => {
                        self.errors.push(e);
                        None
                    }
                }
            }
        }
    }

    fn fragment(&mut self, object: &'a ObjectDef, set: &'a SelectionSet) -> Option<Fields> {
        let mut out: Option<Fields> = None;
        for item in &set.items {
            if let Some(part) = self.selection(object, &item.node) {
                let acc = out.get_or_insert_with(Fields::new);
                for (k, v) in part {
                    acc.entry(k).or_insert(v);
                }
            }
        }
        out
    }

    fn field(
        &mut self,
        object: &'a ObjectDef,
        f: &'a async_graphql_parser::types::Field,
    ) -> Result<Option<FieldInfo>, String> {
        let name = f.name.node.as_str();
        let is_root = matches!(
            object.name.as_str(),
            "query_root" | "mutation_root" | "subscription_root"
        );
        let ty = match (object.fields.get(name), name) {
            (Some(def), _) => def.ty.clone(),
            (None, "__typename") => TypeRef::nn("String"),
            (None, "__schema") if is_root => TypeRef::nn("__Schema"),
            (None, "__type") if is_root => TypeRef::nn("__Type"),
            (None, _) => {
                return Err(self.diagnosis(format!(
                    "field '{name}' not found in object '{}'",
                    object.name
                )));
            }
        };
        let base = ty.base();
        let has_selection = !f.selection_set.node.items.is_empty();
        match self.schema.get(base) {
            None => Err(self.diagnosis(format!("type '{base}' not found in the schema"))),
            Some(TypeDef::InputObject(_)) => {
                Err(self.diagnosis(format!("input object '{base}' cannot be used for output")))
            }
            Some(TypeDef::Scalar(_)) if has_selection => {
                Err(self.diagnosis(format!("scalar '{base}' does not accept a selection set")))
            }
            Some(TypeDef::Enum(_)) if has_selection => {
                Err(self.diagnosis(format!("enum '{base}' does not accept a selection set")))
            }
            Some(TypeDef::Scalar(_) | TypeDef::Enum(_)) => Ok(Some(FieldInfo::Leaf(ty))),
            Some(TypeDef::Object(_)) if !has_selection => {
                Err(self.diagnosis(format!("object of type '{base}' must have a selection set")))
            }
            Some(TypeDef::Object(o)) => {
                let fields = self.object_selection_set(o, &f.selection_set.node)?;
                Ok(Some(FieldInfo::Object(ty, fields)))
            }
            // V2 does not analyse unions and interfaces (remote schemas): no info, no error.
            _ => Ok(None),
        }
    }

    /// V2's `unionWithM mergeFields old new`: each key of `new` is merged into `old`, the
    /// merge seeing the new value first.
    fn union(&self, mut old: Fields, new: Fields) -> Result<Fields, String> {
        for (k, v) in new {
            let merged = match old.shift_remove(&k) {
                None => v,
                Some(prev) => self.merge(&k, v, prev)?,
            };
            old.insert(k, merged);
        }
        Ok(old)
    }

    fn merge(&self, name: &str, a: FieldInfo, b: FieldInfo) -> Result<FieldInfo, String> {
        let clash = || {
            self.diagnosis(format!(
                "field '{name}' seen with two different types: {} and {}",
                show_type(a.ty()),
                show_type(b.ty())
            ))
        };
        match (&a, &b) {
            (FieldInfo::Leaf(t1), FieldInfo::Leaf(t2)) if t1 == t2 => Ok(a),
            (FieldInfo::Object(t1, _), FieldInfo::Object(t2, _)) if t1 == t2 => {
                let (FieldInfo::Object(t, s1), FieldInfo::Object(_, s2)) = (a, b) else {
                    unreachable!("matched above")
                };
                Ok(FieldInfo::Object(t, self.union(s1, s2)?))
            }
            _ => Err(clash()),
        }
    }
}

/// Haskell's derived `Show` of the parser's `GType`, which V2 puts in its messages.
fn show_type(ty: &TypeRef) -> String {
    let (inner, nullable) = match ty {
        TypeRef::NonNull(t) => (t.as_ref(), "False"),
        other => (other, "True"),
    };
    let nullability = format!("(Nullability {{unNullability = {nullable}}})");
    match inner {
        TypeRef::Named(n) => format!("TypeNamed {nullability} (Name {{unName = \"{n}\"}})"),
        TypeRef::List(t) => format!("TypeList {nullability} ({})", show_type(t)),
        TypeRef::NonNull(t) => show_type(t),
    }
}
