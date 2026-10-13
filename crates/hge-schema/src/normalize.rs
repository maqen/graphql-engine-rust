//! Validation and normalisation of a GraphQL request against a [`Schema`].
//!
//! Produces a tree of [`SelectedField`]s: fragments inlined, `@skip`/`@include` applied, fields
//! with the same response key merged, arguments coerced to JSON values (variables
//! substituted). Lowering and introspection work on this tree only.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use async_graphql_parser::Positioned;
use async_graphql_parser::types::{
    Directive, DocumentOperations, ExecutableDocument, FragmentDefinition, OperationDefinition,
    OperationType, Selection, SelectionSet,
};
use async_graphql_value::{Name, Value as GqlValue};
use hge_core::{Code, Error};
use hge_ir::OperationKind;
use indexmap::{IndexMap, IndexSet};
use serde_json::{Map, Value as Json};

use crate::introspection;
use crate::model::*;
use crate::scalars::{self, BuiltinScalar};

/// A selected field after normalisation.
#[derive(Debug, Clone)]
pub struct SelectedField<'s> {
    pub alias: String,
    pub name: String,
    pub kind: Selected<'s>,
    /// Coerced arguments that were provided (or have defaults), in schema order.
    pub args: IndexMap<String, Json>,
    /// Sub-selection (empty for leaf fields and fields of an abstract type).
    pub selection: Vec<SelectedField<'s>>,
    /// For a field of an interface or union type: the sub-selection for each possible
    /// object type, in schema order.
    pub by_type: Vec<(&'s str, Vec<SelectedField<'s>>)>,
    /// The arguments as written in the query (variables unresolved). Only kept for remote
    /// schema fields, which forward them.
    pub raw_args: Vec<(String, GqlValue)>,
    /// V2-style JSON path of this field (`$.selectionSet.author`).
    pub path: String,
}

#[derive(Debug, Clone, Copy)]
pub enum Selected<'s> {
    /// `__typename` on the given object type.
    Typename(&'s str),
    Field(&'s FieldDef),
}

impl SelectedField<'_> {
    pub fn arg(&self, name: &str) -> Option<&Json> {
        self.args.get(name)
    }

    pub fn arg_path(&self, name: &str) -> String {
        format!("{}.args.{}", self.path, name)
    }
}

#[derive(Debug)]
pub struct NormalizedOperation<'s> {
    pub kind: OperationKind,
    pub root_type: &'s str,
    pub fields: Vec<SelectedField<'s>>,
    /// The operation carries `@_multiple_top_level_fields` (subscriptions only).
    pub multiple_top_level_fields: bool,
    /// The operation's variables, coerced.
    pub variables: Variables,
    /// The request's `operationName`.
    pub operation_name: Option<String>,
}

/// Select the operation, coerce variables, and normalise its selection set.
pub fn normalize<'s>(
    schema: &'s Schema,
    doc: &ExecutableDocument,
    operation_name: Option<&str>,
    variables: &Map<String, Json>,
) -> Result<NormalizedOperation<'s>, Error> {
    let op = select_operation(doc, operation_name)?;
    let (kind, root_type) = match op.ty {
        OperationType::Query => (OperationKind::Query, schema.query_type.as_str()),
        OperationType::Mutation => match &schema.mutation_type {
            Some(t) => (OperationKind::Mutation, t.as_str()),
            None => return Err(Error::validation("no mutations exist")),
        },
        OperationType::Subscription => match &schema.subscription_type {
            Some(t) => (OperationKind::Subscription, t.as_str()),
            None => return Err(Error::validation("no subscriptions exist")),
        },
    };
    let multiple_top_level_fields = operation_directives(op)?;
    let vars = check_operation(doc, op, variables)?;
    let cx = Context {
        schema,
        fragments: &doc.fragments,
        variables: vars,
        budget: Cell::new(MAX_SELECTIONS),
    };
    let mut visiting = HashSet::new();
    let fields =
        cx.collect_and_normalize(root_type, &[&op.selection_set.node], "$", 0, &mut visiting)?;
    Ok(NormalizedOperation {
        kind,
        root_type,
        fields,
        multiple_top_level_fields,
        variables: cx.variables,
        operation_name: operation_name.map(str::to_owned),
    })
}

/// Check the operation's directives; returns whether `@_multiple_top_level_fields` is present.
fn operation_directives(op: &OperationDefinition) -> Result<bool, Error> {
    let mut multiple = false;
    for d in &op.directives {
        if d.node.name.node.as_str() == MULTIPLE_TOP_LEVEL_FIELDS {
            if op.ty != OperationType::Subscription {
                let on = match op.ty {
                    OperationType::Query => "a query",
                    _ => "a mutation",
                };
                return Err(Error::validation(format!(
                    "directive '{MULTIPLE_TOP_LEVEL_FIELDS}' is not allowed on {on}"
                ))
                .with_path("$"));
            }
            multiple = true;
        }
    }
    Ok(multiple)
}

/// V2's hidden, test-only subscription directive.
const MULTIPLE_TOP_LEVEL_FIELDS: &str = "_multiple_top_level_fields";

fn select_operation<'d>(
    doc: &'d ExecutableDocument,
    operation_name: Option<&str>,
) -> Result<&'d OperationDefinition, Error> {
    match (&doc.operations, operation_name) {
        (DocumentOperations::Single(op), None) => Ok(&op.node),
        (DocumentOperations::Single(op), Some(_)) => Ok(&op.node),
        (DocumentOperations::Multiple(ops), Some(name)) => {
            ops.get(name).map(|op| &op.node).ok_or_else(|| {
                Error::validation(format!(
                    "no such operation found in the document: \"{name}\""
                ))
            })
        }
        (DocumentOperations::Multiple(ops), None) if ops.len() == 1 => {
            Ok(&ops.values().next().expect("one op").node)
        }
        (DocumentOperations::Multiple(_), None) => Err(Error::validation(
            "exactly one operation has to be present in the document when operationName is not specified",
        )),
    }
}

/// Variable values as given (`None`: not provided and no default), and whether the variable
/// has a non-null default value. Like V2, values are coerced where they are used, so errors carry the
/// path of the use.
pub type Variables = HashMap<String, (TypeRef, Option<Json>, bool)>;

fn coerce_variables(
    op: &OperationDefinition,
    provided: &Map<String, Json>,
) -> Result<Variables, Error> {
    let mut out = HashMap::new();
    // V2 checks every definition's value first, then for duplicates.
    let mut duplicate = None;
    for def in &op.variable_definitions {
        let def = &def.node;
        let name = def.name.node.to_string();
        let ty = TypeRef::from_ast(&def.var_type.node);
        let path = format!("$.variableValues.{name}");
        // V2 only type-checks a variable where it is used (against the expected type), so an
        // unused variable may name an output type or a type the schema doesn't have.
        if out.contains_key(&name) {
            duplicate.get_or_insert_with(|| name.clone());
        }
        let value = match provided.get(&name) {
            Some(Json::Null) if ty.is_non_null() => {
                return Err(Error::validation(format!(
                    "null value found for non-nullable type: \"{ty}\""
                ))
                .with_path("$"));
            }
            Some(v) => Some(v.clone()),
            None => match &def.default_value {
                Some(d) => Some(
                    d.node
                        .clone()
                        .into_json()
                        .map_err(|e| Error::validation(e.to_string()).with_path(path.clone()))?,
                ),
                None if ty.is_non_null() => {
                    return Err(Error::validation(format!(
                        "expecting a value for non-nullable variable: \"{name}\""
                    ))
                    .with_path("$"));
                }
                None => None,
            },
        };
        // Only a non-null default lets a nullable variable fill a non-null position.
        let has_default = def
            .default_value
            .as_ref()
            .is_some_and(|d| !matches!(d.node, async_graphql_value::ConstValue::Null));
        out.insert(name, (ty, value, has_default));
    }
    if let Some(name) = duplicate {
        return Err(Error::new(
            Code::ParseFailed,
            format!("multiple definitions for variable \"{name}\""),
        )
        .with_path("$"));
    }
    Ok(out)
}

/// V2's checks before it parses any field, in its order: inlining fragments (undefined
/// fragments and cycles), the provided variable values, then variables used but not declared
/// and provided values the operation doesn't use.
fn check_operation(
    doc: &ExecutableDocument,
    op: &OperationDefinition,
    provided: &Map<String, Json>,
) -> Result<Variables, Error> {
    let mut inline = Inline {
        doc,
        used: IndexSet::new(),
        inlined: HashSet::new(),
        stack: vec![],
    };
    for d in &op.directives {
        directive_variables(&d.node, &mut inline.used);
    }
    inline.walk(&op.selection_set.node)?;
    let variables = coerce_variables(op, provided)?;
    if let Some(name) = inline.used.iter().find(|v| !variables.contains_key(**v)) {
        return Err(Error::validation(format!("unbound variable \"{name}\"")).with_path("$"));
    }
    let used: HashSet<&str> = inline.used.iter().copied().collect();
    let mut unexpected: Vec<&str> = provided
        .keys()
        .map(String::as_str)
        .filter(|k| !used.contains(k))
        .collect();
    if !unexpected.is_empty() {
        // V2 lists them from a `HashSet`.
        hge_core::sort_v2_hash_order(&mut unexpected);
        return Err(Error::validation(format!(
            "unexpected variables in variableValues: {}",
            unexpected.join(", ")
        ))
        .with_path("$"));
    }
    Ok(variables)
}

/// A walk over the operation as V2 inlines its fragments (`Hasura.GraphQL.Execute.Inline`),
/// collecting the variables it uses.
struct Inline<'d> {
    doc: &'d ExecutableDocument,
    /// Variables used, in order of first use.
    used: IndexSet<&'d str>,
    /// Fragments already inlined (V2 caches them, so they are walked once).
    inlined: HashSet<&'d str>,
    /// Fragments being inlined, outermost first.
    stack: Vec<&'d str>,
}

impl<'d> Inline<'d> {
    /// Walk `root` depth first (with an explicit stack: fragment chains can nest deeply).
    /// V2's error paths name each field (not its alias) and fragment under `selectionSet`.
    fn walk(&mut self, root: &'d SelectionSet) -> Result<(), Error> {
        struct Frame<'d> {
            set: &'d SelectionSet,
            next: usize,
            path: String,
            /// The fragment whose body this is.
            fragment: Option<&'d str>,
        }
        let mut frames = vec![Frame {
            set: root,
            next: 0,
            path: "$".to_owned(),
            fragment: None,
        }];
        while let Some(frame) = frames.last_mut() {
            let Some(item) = frame.set.items.get(frame.next) else {
                if let Some(name) = frames.pop().and_then(|f| f.fragment) {
                    self.stack.pop();
                    self.inlined.insert(name);
                }
                continue;
            };
            frame.next += 1;
            let path = &frame.path;
            let child = match &item.node {
                Selection::Field(f) => {
                    let f = &f.node;
                    for (_, v) in &f.arguments {
                        value_variables(&v.node, &mut self.used);
                    }
                    for d in &f.directives {
                        directive_variables(&d.node, &mut self.used);
                    }
                    Frame {
                        set: &f.selection_set.node,
                        next: 0,
                        path: format!("{path}.selectionSet.{}", f.name.node),
                        fragment: None,
                    }
                }
                Selection::FragmentSpread(spread) => {
                    let spread = &spread.node;
                    let name = spread.fragment_name.node.as_str();
                    let path = format!("{path}.selectionSet");
                    for d in &spread.directives {
                        directive_variables(&d.node, &mut self.used);
                    }
                    if self.inlined.contains(name) {
                        continue;
                    }
                    if let Some(at) = self.stack.iter().position(|f| *f == name) {
                        return Err(Error::validation(format!(
                            "the fragment definition(s) {} form a cycle",
                            english_list(&self.stack[at..])
                        ))
                        .with_path(path));
                    }
                    let Some(fragment) = self.doc.fragments.get(&spread.fragment_name.node) else {
                        return Err(Error::validation(format!(
                            "reference to undefined fragment \"{name}\""
                        ))
                        .with_path(path));
                    };
                    // V2 drops the definition's own directives.
                    self.stack.push(name);
                    Frame {
                        set: &fragment.node.selection_set.node,
                        next: 0,
                        path: format!("{path}.{name}"),
                        fragment: Some(name),
                    }
                }
                Selection::InlineFragment(inline) => {
                    let inline = &inline.node;
                    for d in &inline.directives {
                        directive_variables(&d.node, &mut self.used);
                    }
                    Frame {
                        set: &inline.selection_set.node,
                        next: 0,
                        path: path.clone(),
                        fragment: None,
                    }
                }
            };
            frames.push(child);
        }
        Ok(())
    }
}

/// `a`, `a and b`, `a, b, and c` (Hasura's `englishList "and"`).
fn english_list(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [one, two] => format!("{one} and {two}"),
        [init @ .., last] => format!("{}, and {last}", init.join(", ")),
    }
}

fn directive_variables<'d>(d: &'d Directive, used: &mut IndexSet<&'d str>) {
    for (_, v) in &d.arguments {
        value_variables(&v.node, used);
    }
}

fn value_variables<'d>(v: &'d GqlValue, used: &mut IndexSet<&'d str>) {
    match v {
        GqlValue::Variable(name) => {
            used.insert(name.as_str());
        }
        GqlValue::List(items) => items.iter().for_each(|i| value_variables(i, used)),
        GqlValue::Object(fields) => fields.values().for_each(|i| value_variables(i, used)),
        _ => {}
    }
}

/// Deepest selection set nesting accepted (fragments can nest deeper than the parser
/// allows, and every level is a recursion here, in lowering and in SQL generation).
const MAX_DEPTH: usize = 100;

/// Selections visited per request, counting each fragment expansion. Repeated fragment
/// spreads grow exponentially with query size; this bounds the work.
const MAX_SELECTIONS: usize = 20_000;

struct Context<'s, 'd> {
    schema: &'s Schema,
    fragments: &'d HashMap<Name, Positioned<FragmentDefinition>>,
    variables: Variables,
    /// Selections left before the query is rejected as too large.
    budget: Cell<usize>,
}

impl<'s, 'd> Context<'s, 'd> {
    /// Collect the fields of `selection_sets` (all on `type_name`), then normalise each
    /// response key.
    fn collect_and_normalize(
        &self,
        type_name: &'s str,
        selection_sets: &[&'d SelectionSet],
        path: &str,
        depth: usize,
        visiting: &mut HashSet<String>,
    ) -> Result<Vec<SelectedField<'s>>, Error> {
        if depth > MAX_DEPTH {
            return Err(Error::validation(format!(
                "the query exceeds the maximum depth of {MAX_DEPTH}"
            ))
            .with_path(path.to_owned()));
        }
        let mut grouped: IndexMap<String, Vec<&'d async_graphql_parser::types::Field>> =
            IndexMap::new();
        for set in selection_sets {
            self.collect(type_name, set, &mut grouped, visiting, path, depth)?;
        }
        let object = self.schema.object(type_name);
        let mut out = Vec::with_capacity(grouped.len());
        for (key, fields) in grouped {
            let first = fields[0];
            let name = first.name.node.as_str();
            // V2 paths name the field, not its alias.
            let field_path = format!("{path}.selectionSet.{name}");
            if fields.iter().any(|f| f.name.node.as_str() != name) {
                return Err(Error::validation(format!(
                    "the response key '{key}' is used for multiple fields with different names"
                ))
                .with_path(field_path));
            }
            if name == "__typename" {
                out.push(SelectedField {
                    alias: key,
                    name: name.to_owned(),
                    kind: Selected::Typename(type_name),
                    args: IndexMap::new(),
                    selection: vec![],
                    by_type: vec![],
                    raw_args: vec![],
                    path: field_path,
                });
                continue;
            }
            let def = object
                .and_then(|o| o.fields.get(name))
                .or_else(|| {
                    (type_name == self.schema.query_type && self.schema.introspection_enabled)
                        .then(|| meta_fields().get(name))
                        .flatten()
                })
                .ok_or_else(|| {
                    Error::validation(format!("field '{name}' not found in type: '{type_name}'"))
                        .with_path(field_path.clone())
                })?;
            let args = self.coerce_arguments(def, first, &field_path)?;
            let base = def.ty.base();
            let base_def = self
                .schema
                .get(base)
                .ok_or_else(|| Error::unexpected(format!("type '{base}' missing from schema")))?;
            let mut by_type = vec![];
            let selection = if base_def.is_leaf() {
                if fields
                    .iter()
                    .any(|f| !f.selection_set.node.items.is_empty())
                {
                    return Err(Error::validation(format!(
                        "unexpected subselection set for non-object field '{name}'"
                    ))
                    .with_path(field_path));
                }
                vec![]
            } else {
                let sets: Vec<&'d SelectionSet> =
                    fields.iter().map(|f| &f.selection_set.node).collect();
                if sets.iter().all(|s| s.items.is_empty()) {
                    return Err(
                        Error::validation(format!("missing selection set for '{base}'"))
                            .with_path(field_path),
                    );
                }
                let base_name: &'s str = base_def.name();
                if base_def.is_abstract() {
                    // Like V2, the selection is parsed against each possible type (so an
                    // unknown field names the object type); charge the budget for the most
                    // expensive pass only.
                    let start = self.budget.get();
                    let mut left = start;
                    for concrete in self.schema.possible_types(base_name) {
                        let concrete: &'s str = concrete;
                        self.budget.set(start);
                        let fields = self.collect_and_normalize(
                            concrete,
                            &sets,
                            &field_path,
                            depth + 1,
                            visiting,
                        )?;
                        by_type.push((concrete, fields));
                        left = left.min(self.budget.get());
                    }
                    self.budget.set(left);
                    vec![]
                } else {
                    self.collect_and_normalize(base_name, &sets, &field_path, depth + 1, visiting)?
                }
            };
            let raw_args = match def.kind {
                FieldKind::Remote(_) => first
                    .arguments
                    .iter()
                    .map(|(n, v)| (n.node.to_string(), v.node.clone()))
                    .collect(),
                _ => vec![],
            };
            out.push(SelectedField {
                alias: key,
                name: name.to_owned(),
                kind: Selected::Field(def),
                args,
                selection,
                by_type,
                raw_args,
                path: field_path,
            });
        }
        Ok(out)
    }

    /// Collect the fields of `set` that apply to the object type `type_name`.
    fn collect(
        &self,
        type_name: &str,
        set: &'d SelectionSet,
        grouped: &mut IndexMap<String, Vec<&'d async_graphql_parser::types::Field>>,
        visiting: &mut HashSet<String>,
        path: &str,
        depth: usize,
    ) -> Result<(), Error> {
        // Fragment nesting recurses too, without adding field depth.
        if depth > MAX_DEPTH {
            return Err(Error::validation(format!(
                "the query exceeds the maximum depth of {MAX_DEPTH}"
            ))
            .with_path(path.to_owned()));
        }
        for sel in &set.items {
            let left = self.budget.get().checked_sub(1).ok_or_else(|| {
                Error::validation(format!(
                    "the query exceeds the maximum of {MAX_SELECTIONS} selections"
                ))
                .with_path(path.to_owned())
            })?;
            self.budget.set(left);
            if !self.should_include(sel.node.directives(), path)? {
                continue;
            }
            match &sel.node {
                Selection::Field(f) => {
                    grouped
                        .entry(f.node.response_key().node.to_string())
                        .or_default()
                        .push(&f.node);
                }
                Selection::InlineFragment(frag) => {
                    if let Some(cond) = &frag.node.type_condition
                        && !self.applies(cond.node.on.node.as_str(), type_name)
                    {
                        continue;
                    }
                    self.collect(
                        type_name,
                        &frag.node.selection_set.node,
                        grouped,
                        visiting,
                        path,
                        depth + 1,
                    )?;
                }
                Selection::FragmentSpread(spread) => {
                    let name = spread.node.fragment_name.node.as_str();
                    let frag = self.fragments.get(name).ok_or_else(|| {
                        Error::validation(format!("reference to undefined fragment '{name}'"))
                            .with_path(path.to_owned())
                    })?;
                    let on = frag.node.type_condition.node.on.node.as_str();
                    if !self.applies(on, type_name) {
                        continue;
                    }
                    if !visiting.insert(name.to_owned()) {
                        return Err(Error::validation(format!(
                            "the fragment definition(s) {name} form a cycle"
                        ))
                        .with_path(path.to_owned()));
                    }
                    let frag_set: &'d SelectionSet = &frag.node.selection_set.node;
                    let result =
                        self.collect(type_name, frag_set, grouped, visiting, path, depth + 1);
                    visiting.remove(name);
                    result?;
                }
            }
        }
        Ok(())
    }

    /// Whether a fragment on type `on` applies to an object of type `type_name`: V2 applies
    /// it when `on` is the object type or an interface it implements, and silently leaves
    /// out any other fragment (even on an unknown type or a union the object belongs to).
    fn applies(&self, on: &str, type_name: &str) -> bool {
        on == type_name
            || self
                .schema
                .object(type_name)
                .is_some_and(|o| o.interfaces.iter().any(|i| i == on))
    }

    fn should_include(
        &self,
        directives: &[Positioned<async_graphql_parser::types::Directive>],
        path: &str,
    ) -> Result<bool, Error> {
        for d in directives {
            let name = d.node.name.node.as_str();
            let skip = match name {
                "skip" => true,
                "include" => false,
                "cached" => continue,
                MULTIPLE_TOP_LEVEL_FIELDS => {
                    return Err(Error::validation(format!(
                        "directive '{MULTIPLE_TOP_LEVEL_FIELDS}' is not allowed on a field"
                    ))
                    .with_path(path.to_owned()));
                }
                other => {
                    return Err(Error::validation(format!(
                        "directive '{other}' is not defined in the schema"
                    ))
                    .with_path(path.to_owned()));
                }
            };
            let arg = d.node.get_argument("if").ok_or_else(|| {
                Error::validation(format!(
                    "missing required argument 'if' for directive '{name}'"
                ))
                .with_path(path.to_owned())
            })?;
            let value = self.resolve(
                &arg.node,
                &TypeRef::nn("Boolean"),
                ValueParser::Strict,
                path,
            )?;
            let cond = value.and_then(|v| v.as_bool()).unwrap_or(false);
            if cond == skip {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn coerce_arguments(
        &self,
        def: &FieldDef,
        field: &async_graphql_parser::types::Field,
        path: &str,
    ) -> Result<IndexMap<String, Json>, Error> {
        for (name, _) in &field.arguments {
            if !def.args.contains_key(name.node.as_str()) {
                return Err(Error::validation(format!(
                    "'{}' has no argument named '{}'",
                    def.name, name.node
                ))
                // V2 rejects unknown arguments before entering `args`.
                .with_path(path));
            }
        }
        let mut out = IndexMap::new();
        for (arg_name, arg_def) in &def.args {
            let arg_path = format!("{path}.args.{arg_name}");
            let given = field
                .arguments
                .iter()
                .find(|(n, _)| n.node.as_str() == arg_name)
                .map(|(_, v)| &v.node);
            let value = match given {
                Some(v) => self.resolve(v, &arg_def.ty, arg_def.parser, &arg_path)?,
                None => None,
            };
            match value {
                Some(v) => {
                    out.insert(arg_name.clone(), v);
                }
                None if arg_def.ty.is_non_null() && arg_def.default_value.is_none() => {
                    return Err(
                        Error::validation(format!("missing required field '{arg_name}'"))
                            .with_path(arg_path),
                    );
                }
                None => {}
            }
        }
        Ok(out)
    }

    /// Coerce a literal (possibly containing variables) to `ty`, parsing built-in scalars with
    /// `parser`. `None` = absent (an unprovided variable).
    fn resolve(
        &self,
        value: &GqlValue,
        ty: &TypeRef,
        parser: ValueParser,
        path: &str,
    ) -> Result<Option<Json>, Error> {
        match value {
            GqlValue::Variable(name) => {
                let (var_ty, var_value, has_default) =
                    self.variables.get(name.as_str()).ok_or_else(|| {
                        Error::validation(format!("unbound variable \"{name}\"")).with_path("$")
                    })?;
                // A nullable variable with a default may be used in a non-null position
                // (GraphQL spec 5.8.5).
                let usage_ty = match ty {
                    TypeRef::NonNull(inner) if *has_default && !var_ty.is_non_null() => inner,
                    _ => ty,
                };
                if !variable_usage_allowed(var_ty, usage_ty) {
                    return Err(Error::validation(format!(
                        "variable '{name}' is declared as '{var_ty}', but used where '{ty}' is expected"
                    ))
                    .with_path(path.to_owned()));
                }
                // V2 hands the variable's JSON value to the parser of the position it is used in.
                var_value
                    .as_ref()
                    .map(|v| coerce_json(self.schema, v, ty, parser, path))
                    .transpose()
            }
            GqlValue::Null => null_value(self.schema, ty, parser, "null", path).map(Some),
            GqlValue::List(items) => match ty.nullable() {
                TypeRef::List(inner) => {
                    let mut out = Vec::with_capacity(items.len());
                    for (i, item) in items.iter().enumerate() {
                        let item_path = format!("{path}[{i}]");
                        match self.resolve(item, inner, parser, &item_path)? {
                            Some(v) => out.push(v),
                            None => out.push(null_value(
                                self.schema,
                                inner,
                                parser,
                                "null",
                                &item_path,
                            )?),
                        }
                    }
                    Ok(Some(Json::Array(out)))
                }
                TypeRef::Named(_) => self.resolve_named(value, ty, parser, path).map(Some),
                TypeRef::NonNull(_) => unreachable!("nullable() strips NonNull"),
            },
            _ => match ty.nullable() {
                TypeRef::List(inner) => {
                    // Input coercion: a single value is a list of one (V2's path says so).
                    Ok(self
                        .resolve(value, inner, parser, &format!("{path}[0]"))?
                        .map(|v| Json::Array(vec![v])))
                }
                _ => self.resolve_named(value, ty, parser, path).map(Some),
            },
        }
    }

    fn resolve_named(
        &self,
        value: &GqlValue,
        ty: &TypeRef,
        parser: ValueParser,
        path: &str,
    ) -> Result<Json, Error> {
        let name = ty.base();
        match self.schema.get(name) {
            Some(TypeDef::InputObject(obj)) => {
                let GqlValue::Object(fields) = value else {
                    return Err(Error::validation(format!(
                        "expected an object for type '{name}', but found {}",
                        describe_gql(value)
                    ))
                    .with_path(path.to_owned()));
                };
                let mut out = Map::new();
                for (k, v) in fields {
                    let field_path = format!("{path}.{k}");
                    let fdef = obj.fields.get(k.as_str()).ok_or_else(|| {
                        Error::validation(format!("field '{k}' not found in type: '{name}'"))
                            .with_path(field_path.clone())
                    })?;
                    // An unprovided variable inside an input object is null (as in V2):
                    // dropping the field would turn `{id: {_eq: $x}}` into "every row".
                    let v = match self.resolve(v, &fdef.ty, fdef.parser, &field_path)? {
                        Some(v) => v,
                        None => {
                            null_value(self.schema, &fdef.ty, fdef.parser, "null", &field_path)?
                        }
                    };
                    out.insert(k.to_string(), v);
                }
                check_required_fields(obj, &out, path)?;
                Ok(Json::Object(out))
            }
            Some(TypeDef::Enum(e)) => match value {
                GqlValue::Enum(v) if e.values.contains_key(v.as_str()) => {
                    Ok(Json::String(v.to_string()))
                }
                GqlValue::Enum(v) => Err(Error::validation(format!(
                    "expected one of the values {} for type '{name}', but found '{v}'",
                    enum_values_list(e),
                ))
                .with_path(path.to_owned())),
                other => Err(Error::validation(format!(
                    "expected an enum value for type '{name}', but found {}",
                    describe_gql(other)
                ))
                .with_path(path.to_owned())),
            },
            Some(TypeDef::Scalar(_)) => {
                // Variables inside a literal (`{body: [{header: $h}]}` for a jsonb column) are
                // replaced by their values, as V2's `valueToJSON` does.
                let json = self.literal_json(value);
                let literal = Literal {
                    found: describe_gql(value),
                    float: matches!(value, GqlValue::Number(n) if n.is_f64()),
                };
                coerce_scalar(name, json, parser, Some(literal), path)
            }
            _ => Err(Error::unexpected(format!("'{name}' is not an input type"))),
        }
    }

    /// A literal as JSON, with variables replaced by their values (null when not given).
    fn literal_json(&self, value: &GqlValue) -> Json {
        match value {
            GqlValue::Variable(name) => self
                .variables
                .get(name.as_str())
                .and_then(|(_, v, _)| v.clone())
                .unwrap_or(Json::Null),
            GqlValue::List(items) => {
                Json::Array(items.iter().map(|i| self.literal_json(i)).collect())
            }
            GqlValue::Object(fields) => Json::Object(
                fields
                    .iter()
                    .map(|(k, v)| (k.to_string(), self.literal_json(v)))
                    .collect(),
            ),
            other => other
                .clone()
                .into_const()
                .and_then(|c| c.into_json().ok())
                .unwrap_or(Json::Null),
        }
    }
}

/// The fields every query root has but does not list (`__schema`, `__type`).
fn meta_fields() -> &'static IndexMap<String, FieldDef> {
    static META: OnceLock<IndexMap<String, FieldDef>> = OnceLock::new();
    META.get_or_init(introspection::meta_fields)
}

/// Variable of type `var_ty` may be used where `expected` is expected (spec 5.8.5, without
/// the default-value relaxation for non-null positions handled by V2 the same way).
fn variable_usage_allowed(var_ty: &TypeRef, expected: &TypeRef) -> bool {
    match (var_ty, expected) {
        (TypeRef::NonNull(v), TypeRef::NonNull(e)) => variable_usage_allowed(v, e),
        (TypeRef::NonNull(v), e) => variable_usage_allowed(v, e),
        (_, TypeRef::NonNull(_)) => false,
        (TypeRef::List(v), TypeRef::List(e)) => variable_usage_allowed(v, e),
        // A variable of the item type may be used where a list is expected (list coercion).
        (v, TypeRef::List(e)) => variable_usage_allowed(v, e),
        (TypeRef::Named(v), TypeRef::Named(e)) => v == e,
        (TypeRef::List(_), TypeRef::Named(_)) => false,
    }
}

/// A null (`found`: how V2 describes it) where `ty` is expected. V2's parsers for non-null
/// types reject it with their own type mismatch (the column parser with "unexpected null
/// value"); other database scalars accept it.
fn null_value(
    schema: &Schema,
    ty: &TypeRef,
    parser: ValueParser,
    found: &str,
    path: &str,
) -> Result<Json, Error> {
    let TypeRef::NonNull(inner) = ty else {
        return Ok(Json::Null);
    };
    let mismatch = |expected: &str, name: &str| {
        Err(Error::validation(format!(
            "expected {expected} for type '{name}', but found {found}"
        ))
        .with_path(path.to_owned()))
    };
    match &**inner {
        TypeRef::List(_) => Err(
            Error::validation(format!("expected a list, but found {found}"))
                .with_path(path.to_owned()),
        ),
        TypeRef::NonNull(_) => null_value(schema, inner, parser, found, path),
        TypeRef::Named(name) => match schema.get(name) {
            Some(TypeDef::InputObject(_)) => mismatch("an object", name),
            Some(TypeDef::Enum(_)) => mismatch("an enum value", name),
            _ if parser == ValueParser::Column => Err(Error::validation(format!(
                "unexpected null value for type '{name}'"
            ))
            .with_path(path.to_owned())),
            _ => match scalars::builtin_kind(name) {
                Some(kind) => mismatch(expected_scalar(kind, parser), name),
                None => Ok(Json::Null),
            },
        },
    }
}

/// What V2's parser for a built-in scalar says it expected.
fn expected_scalar(kind: BuiltinScalar, parser: ValueParser) -> &'static str {
    match (kind, parser) {
        (BuiltinScalar::Int, ValueParser::NonNegativeInt) => "a non-negative 32-bit integer",
        (BuiltinScalar::Int, ValueParser::BigInt) => {
            "a 32-bit integer, or a 64-bit integer represented as a string"
        }
        (BuiltinScalar::Int, _) => "a 32-bit integer",
        (BuiltinScalar::Float, _) => "a float",
        (BuiltinScalar::String, _) => "a string",
        (BuiltinScalar::Boolean, _) => "a boolean",
        (BuiltinScalar::Id, _) => "a String or a 32-bit integer",
    }
}

fn check_required_fields(
    obj: &InputObjectDef,
    given: &Map<String, Json>,
    path: &str,
) -> Result<(), Error> {
    for (fname, fdef) in &obj.fields {
        if fdef.ty.is_non_null() && fdef.default_value.is_none() && !given.contains_key(fname) {
            return Err(
                // V2 points at the missing field.
                Error::validation(format!("missing required field '{fname}'"))
                    .with_path(format!("{path}.{fname}")),
            );
        }
    }
    Ok(())
}

fn enum_values_list(e: &EnumDef) -> String {
    let list = e
        .values
        .keys()
        .map(|v| format!("'{v}'"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{list}]")
}

fn describe_gql(v: &GqlValue) -> &'static str {
    match v {
        GqlValue::Variable(_) => "a variable",
        GqlValue::Null => "null",
        GqlValue::Number(n) if n.is_f64() => "a float",
        GqlValue::Number(_) => "an integer",
        GqlValue::String(_) => "a string",
        GqlValue::Boolean(_) => "a boolean",
        GqlValue::Binary(_) => "binary data",
        GqlValue::Enum(_) => "an enum value",
        GqlValue::List(_) => "a list",
        GqlValue::Object(_) => "an object",
    }
}

fn describe_json(v: &Json) -> &'static str {
    match v {
        Json::Null => "null",
        Json::Bool(_) => "a boolean",
        Json::Number(_) => "a number",
        Json::String(_) => "a string",
        Json::Array(_) => "a list",
        Json::Object(_) => "an object",
    }
}

/// Coerce a JSON variable value to `ty` (variables arrive as JSON).
pub(crate) fn coerce_json(
    schema: &Schema,
    value: &Json,
    ty: &TypeRef,
    parser: ValueParser,
    path: &str,
) -> Result<Json, Error> {
    match (value, ty) {
        (Json::Null, _) => null_value(schema, ty, parser, "null", path),
        (_, TypeRef::NonNull(inner)) => coerce_json(schema, value, inner, parser, path),
        (Json::Array(items), TypeRef::List(inner)) => items
            .iter()
            .enumerate()
            .map(|(i, v)| coerce_json(schema, v, inner, parser, &format!("{path}[{i}]")))
            .collect::<Result<Vec<_>, _>>()
            .map(Json::Array),
        (_, TypeRef::List(inner)) => Ok(Json::Array(vec![coerce_json(
            schema,
            value,
            inner,
            parser,
            &format!("{path}[0]"),
        )?])),
        (_, TypeRef::Named(name)) => match schema.get(name) {
            Some(TypeDef::Scalar(_)) => coerce_scalar(name, value.clone(), parser, None, path),
            Some(TypeDef::Enum(e)) => match value {
                Json::String(s) if e.values.contains_key(s) => Ok(value.clone()),
                Json::String(v) if crate::tables::is_valid_name(v) => {
                    Err(Error::validation(format!(
                        "expected one of the values {} for type '{name}', but found '{v}'",
                        enum_values_list(e),
                    ))
                    .with_path(path.to_owned()))
                }
                other => Err(Error::validation(format!(
                    "expected an enum value for type '{name}', but found {}",
                    describe_json(other)
                ))
                .with_path(path.to_owned())),
            },
            Some(TypeDef::InputObject(obj)) => {
                let Json::Object(fields) = value else {
                    return Err(Error::validation(format!(
                        "expected an object for type '{name}', but found {}",
                        describe_json(value)
                    ))
                    .with_path(path.to_owned()));
                };
                let mut out = Map::new();
                for (k, v) in fields {
                    let field_path = format!("{path}.{k}");
                    let fdef = obj.fields.get(k).ok_or_else(|| {
                        Error::validation(format!("field '{k}' not found in type: '{name}'"))
                            .with_path(field_path.clone())
                    })?;
                    out.insert(
                        k.clone(),
                        coerce_json(schema, v, &fdef.ty, fdef.parser, &field_path)?,
                    );
                }
                check_required_fields(obj, &out, path)?;
                Ok(Json::Object(out))
            }
            _ => Err(Error::validation(format!("'{name}' is not an input type"))
                .with_path(path.to_owned())),
        },
    }
}

/// A scalar given as a GraphQL literal rather than as JSON: how V2 describes it, and whether
/// it was a float literal (a type mismatch for `Int`, where a JSON `2.0` is fine).
struct Literal {
    found: &'static str,
    float: bool,
}

/// Parse a non-null scalar value as V2's parser for this position does (see [`ValueParser`]).
fn coerce_scalar(
    name: &str,
    value: Json,
    parser: ValueParser,
    literal: Option<Literal>,
    path: &str,
) -> Result<Json, Error> {
    let Some(kind) = scalars::builtin_kind(name) else {
        // Custom scalars (database types) are validated by the database.
        return Ok(value);
    };
    let found = literal
        .as_ref()
        .map_or_else(|| describe_json(&value), |l| l.found);
    let mismatch = || {
        Err(Error::validation(format!(
            "expected {} for type '{name}', but found {found}",
            expected_scalar(kind, parser)
        ))
        .with_path(path.to_owned()))
    };
    let column = parser == ValueParser::Column;
    // V2's column parser (`parsePGValue`) passes a string it cannot parse to Postgres as text.
    if column
        && value.is_string()
        && matches!(
            kind,
            BuiltinScalar::Int | BuiltinScalar::Float | BuiltinScalar::Boolean
        )
    {
        return Ok(value);
    }
    match kind {
        BuiltinScalar::Int => match &value {
            // The column parser goes through JSON, so a float literal is a range error there.
            Json::Number(_) if literal.as_ref().is_some_and(|l| l.float) && !column => mismatch(),
            Json::Number(n) if parser == ValueParser::NonNegativeInt && n.as_f64() < Some(0.0) => {
                mismatch()
            }
            Json::Number(n) => {
                let fits = |f: f64| {
                    if parser == ValueParser::BigInt {
                        // i64::MAX as f64 rounds up to 2^63.
                        (-(2f64.powi(63))..2f64.powi(63)).contains(&f)
                    } else {
                        f.abs() <= i32::MAX as f64 && f >= i32::MIN as f64
                    }
                };
                match (n.as_i64(), n.as_f64()) {
                    (Some(i), _) if parser == ValueParser::BigInt || i32::try_from(i).is_ok() => {
                        Ok(value)
                    }
                    (None, Some(f)) if f.fract() == 0.0 && fits(f) => Ok(Json::from(f as i64)),
                    // V2 parses the number first, then rejects it with this message (and code).
                    _ => Err(Error::new(
                        Code::ParseFailed,
                        format!(
                            "The value {} lies outside the bounds or is not an integer. Maybe it \
                             is a float, or is there integer overflow?",
                            haskell_scientific(n)
                        ),
                    )
                    .with_path(path.to_owned())),
                }
            }
            // `Data.Text.Read.decimal`: unsigned digits only.
            Json::String(s)
                if parser == ValueParser::BigInt
                    && !s.is_empty()
                    && s.bytes().all(|b| b.is_ascii_digit()) =>
            {
                match s.parse::<i64>() {
                    Ok(i) => Ok(Json::from(i)),
                    Err(_) => mismatch(),
                }
            }
            _ => mismatch(),
        },
        BuiltinScalar::Float => match &value {
            Json::Number(_) => Ok(value),
            _ => mismatch(),
        },
        BuiltinScalar::String => match &value {
            Json::String(_) => Ok(value),
            _ => mismatch(),
        },
        BuiltinScalar::Boolean => match &value {
            Json::Bool(_) => Ok(value),
            _ => mismatch(),
        },
        BuiltinScalar::Id => match &value {
            Json::String(_) => Ok(value),
            Json::Number(n) if n.is_i64() || n.is_u64() => Ok(Json::String(n.to_string())),
            _ => mismatch(),
        },
    }
}

/// A number as Haskell's `Scientific` shows it (V2 quotes values this way in errors): fixed
/// notation for `0.1 <= |x| < 10^7`, else `d.ddde<n>`.
fn haskell_scientific(n: &serde_json::Number) -> String {
    let text = n.to_string();
    let (sign, text) = match text.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", text.as_str()),
    };
    let (mantissa, exp) = match text.split_once(['e', 'E']) {
        Some((m, e)) => (m, e.parse::<i64>().unwrap_or(0)),
        None => (text, 0),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let all: String = format!("{int}{frac}");
    let lead = all.len() - all.trim_start_matches('0').len();
    let digits = all.trim_matches('0');
    if digits.is_empty() {
        return format!("{sign}0.0");
    }
    // value = 0.<digits> × 10^e
    let e = int.len() as i64 - lead as i64 + exp;
    if !(0..=7).contains(&e) {
        let (first, rest) = digits.split_at(1);
        let rest = if rest.is_empty() { "0" } else { rest };
        return format!("{sign}{first}.{rest}e{}", e - 1);
    }
    if e == 0 {
        return format!("{sign}0.{digits}");
    }
    let e = e as usize;
    if digits.len() <= e {
        format!("{sign}{digits}{}.0", "0".repeat(e - digits.len()))
    } else {
        format!("{sign}{}.{}", &digits[..e], &digits[e..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variable_usage_rules() {
        let int = TypeRef::named("Int");
        assert!(variable_usage_allowed(&int.clone().non_null(), &int));
        assert!(!variable_usage_allowed(&int, &int.clone().non_null()));
        assert!(variable_usage_allowed(&int, &int.clone().list()));
        assert!(!variable_usage_allowed(&TypeRef::named("String"), &int));
    }

    #[test]
    fn numbers_shown_like_haskell_scientific() {
        let show = |v: serde_json::Value| match v {
            serde_json::Value::Number(n) => super::haskell_scientific(&n),
            _ => unreachable!(),
        };
        assert_eq!(show(serde_json::json!(2147483648u64)), "2.147483648e9");
        assert_eq!(show(serde_json::json!(1.5)), "1.5");
        assert_eq!(show(serde_json::json!(0.5)), "0.5");
        assert_eq!(show(serde_json::json!(0.05)), "5.0e-2");
        assert_eq!(show(serde_json::json!(-3000000000i64)), "-3.0e9");
        assert_eq!(show(serde_json::json!(12.25)), "12.25");
    }

    #[test]
    fn names_starting_with_keywords_parse() {
        // Our vendored parser only matches `null`/`true`/`false` as whole words (V2 parses these).
        let doc = "mutation { f(objects: [{nullName: \"2\"}], cols: [nullName, trueish, falsey], n: null, t: true) }";
        assert!(async_graphql_parser::parse_query(doc).is_ok());
        assert!(async_graphql_parser::parse_query("{ f(a: nullx) }").is_ok());
    }
}
