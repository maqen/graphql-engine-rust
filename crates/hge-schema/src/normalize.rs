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
    DocumentOperations, ExecutableDocument, FragmentDefinition, OperationDefinition, OperationType,
    Selection, SelectionSet,
};
use async_graphql_value::{Name, Value as GqlValue};
use hge_core::Error;
use hge_ir::OperationKind;
use indexmap::IndexMap;
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
    /// Sub-selection (empty for leaf fields).
    pub selection: Vec<SelectedField<'s>>,
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
    let vars = coerce_variables(schema, op, variables)?;
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
/// has a default value. Like V2, values are coerced where they are used, so errors carry the
/// path of the use.
type Variables = HashMap<String, (TypeRef, Option<Json>, bool)>;

fn coerce_variables(
    schema: &Schema,
    op: &OperationDefinition,
    provided: &Map<String, Json>,
) -> Result<Variables, Error> {
    let mut out = HashMap::new();
    for def in &op.variable_definitions {
        let def = &def.node;
        let name = def.name.node.to_string();
        let ty = TypeRef::from_ast(&def.var_type.node);
        let path = format!("$.variableValues.{name}");
        match schema.get(ty.base()) {
            Some(t) if t.is_input() => {}
            Some(_) => {
                return Err(Error::validation(format!(
                    "variable '{name}' is declared as '{ty}', but '{}' is not an input type",
                    ty.base()
                ))
                .with_path(path));
            }
            None => {
                return Err(Error::validation(format!(
                    "unknown type '{}' for variable '{name}'",
                    ty.base()
                ))
                .with_path(path));
            }
        }
        let value = match provided.get(&name) {
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
        out.insert(name, (ty, value, def.default_value.is_some()));
    }
    Ok(out)
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
                self.collect_and_normalize(base_name, &sets, &field_path, depth + 1, visiting)?
            };
            out.push(SelectedField {
                alias: key,
                name: name.to_owned(),
                kind: Selected::Field(def),
                args,
                selection,
                path: field_path,
            });
        }
        Ok(out)
    }

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
                    if let Some(cond) = &frag.node.type_condition {
                        let on = cond.node.on.node.as_str();
                        self.check_fragment_type(on, type_name, path)?;
                        if on != type_name {
                            continue;
                        }
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
                    self.check_fragment_type(on, type_name, path)?;
                    if on != type_name {
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

    fn check_fragment_type(&self, on: &str, type_name: &str, path: &str) -> Result<(), Error> {
        match self.schema.get(on) {
            Some(TypeDef::Object(_)) if on == type_name => Ok(()),
            Some(TypeDef::Object(_)) => Err(Error::validation(format!(
                "cannot spread fragment on type '{on}' within type '{type_name}'"
            ))
            .with_path(path.to_owned())),
            Some(_) => Err(Error::validation(format!(
                "fragments can only be defined on object types, not on '{on}'"
            ))
            .with_path(path.to_owned())),
            None => Err(Error::validation(format!(
                "fragment type condition refers to unknown type '{on}'"
            ))
            .with_path(path.to_owned())),
        }
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
            let value = self.resolve(&arg.node, &TypeRef::nn("Boolean"), path)?;
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
                .with_path(format!("{path}.args.{}", name.node)));
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
                Some(v) => self.resolve(v, &arg_def.ty, &arg_path)?,
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

    /// Coerce a literal (possibly containing variables) to `ty`. `None` = absent (an
    /// unprovided variable).
    fn resolve(&self, value: &GqlValue, ty: &TypeRef, path: &str) -> Result<Option<Json>, Error> {
        match value {
            GqlValue::Variable(name) => {
                let (var_ty, var_value, has_default) =
                    self.variables.get(name.as_str()).ok_or_else(|| {
                        Error::validation(format!("unbound variable '{name}'"))
                            .with_path(path.to_owned())
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
                if ty.is_non_null() && matches!(var_value, Some(Json::Null)) {
                    return Err(null_error(ty, path));
                }
                var_value
                    .as_ref()
                    .map(|v| coerce_json(self.schema, v, var_ty, path))
                    .transpose()
            }
            GqlValue::Null => {
                if ty.is_non_null() {
                    Err(null_error(ty, path))
                } else {
                    Ok(Some(Json::Null))
                }
            }
            GqlValue::List(items) => match ty.nullable() {
                TypeRef::List(inner) => {
                    let mut out = Vec::with_capacity(items.len());
                    for (i, item) in items.iter().enumerate() {
                        let item_path = format!("{path}[{i}]");
                        match self.resolve(item, inner, &item_path)? {
                            Some(v) => out.push(v),
                            None if inner.is_non_null() => {
                                return Err(null_error(inner, &item_path));
                            }
                            None => out.push(Json::Null),
                        }
                    }
                    Ok(Some(Json::Array(out)))
                }
                TypeRef::Named(_) => self.resolve_named(value, ty, path).map(Some),
                TypeRef::NonNull(_) => unreachable!("nullable() strips NonNull"),
            },
            _ => match ty.nullable() {
                TypeRef::List(inner) => {
                    // Input coercion: a single value is a list of one.
                    Ok(self
                        .resolve(value, inner, path)?
                        .map(|v| Json::Array(vec![v])))
                }
                _ => self.resolve_named(value, ty, path).map(Some),
            },
        }
    }

    fn resolve_named(&self, value: &GqlValue, ty: &TypeRef, path: &str) -> Result<Json, Error> {
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
                    match self.resolve(v, &fdef.ty, &field_path)? {
                        Some(v) => {
                            out.insert(k.to_string(), v);
                        }
                        None if fdef.ty.is_non_null() => {
                            return Err(null_error(&fdef.ty, &field_path));
                        }
                        None => {
                            out.insert(k.to_string(), Json::Null);
                        }
                    }
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
                let json = value
                    .clone()
                    .into_const()
                    .and_then(|c| c.into_json().ok())
                    .ok_or_else(|| {
                        Error::validation(format!("unexpected value for type '{name}'"))
                            .with_path(path.to_owned())
                    })?;
                coerce_scalar(name, json, path)
            }
            _ => Err(Error::unexpected(format!("'{name}' is not an input type"))),
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

fn null_error(ty: &TypeRef, path: &str) -> Error {
    Error::validation(format!(
        "unexpected null value for type '{}'",
        ty.nullable()
    ))
    .with_path(path.to_owned())
}

fn check_required_fields(
    obj: &InputObjectDef,
    given: &Map<String, Json>,
    path: &str,
) -> Result<(), Error> {
    for (fname, fdef) in &obj.fields {
        if fdef.ty.is_non_null() && fdef.default_value.is_none() && !given.contains_key(fname) {
            return Err(
                Error::validation(format!("missing required field '{fname}'"))
                    .with_path(path.to_owned()),
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
    path: &str,
) -> Result<Json, Error> {
    match (value, ty) {
        (Json::Null, TypeRef::NonNull(_)) => Err(null_error(ty, path)),
        (Json::Null, _) => Ok(Json::Null),
        (_, TypeRef::NonNull(inner)) => coerce_json(schema, value, inner, path),
        (Json::Array(items), TypeRef::List(inner)) => items
            .iter()
            .enumerate()
            .map(|(i, v)| coerce_json(schema, v, inner, &format!("{path}[{i}]")))
            .collect::<Result<Vec<_>, _>>()
            .map(Json::Array),
        (_, TypeRef::List(inner)) => {
            Ok(Json::Array(vec![coerce_json(schema, value, inner, path)?]))
        }
        (_, TypeRef::Named(name)) => match schema.get(name) {
            Some(TypeDef::Scalar(_)) => coerce_scalar(name, value.clone(), path),
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
                    out.insert(k.clone(), coerce_json(schema, v, &fdef.ty, &field_path)?);
                }
                check_required_fields(obj, &out, path)?;
                Ok(Json::Object(out))
            }
            _ => Err(Error::validation(format!("'{name}' is not an input type"))
                .with_path(path.to_owned())),
        },
    }
}

fn coerce_scalar(name: &str, value: Json, path: &str) -> Result<Json, Error> {
    let err = |expected: &str| {
        Error::validation(format!(
            "expected {expected} for type '{name}', but found {}",
            describe_json(&value)
        ))
        .with_path(path.to_owned())
    };
    match scalars::builtin_kind(name) {
        Some(BuiltinScalar::Int) => match &value {
            Json::Number(n) if n.as_i64().is_some_and(|i| i32::try_from(i).is_ok()) => Ok(value),
            Json::Number(n)
                if n.as_f64()
                    .is_some_and(|f| f.fract() == 0.0 && f.abs() <= i32::MAX as f64) =>
            {
                Ok(Json::from(n.as_f64().unwrap_or_default() as i64))
            }
            _ => Err(err("a 32-bit integer")),
        },
        Some(BuiltinScalar::Float) => match &value {
            Json::Number(_) => Ok(value),
            _ => Err(err("a float")),
        },
        Some(BuiltinScalar::String) => match &value {
            Json::String(_) => Ok(value),
            _ => Err(err("a string")),
        },
        Some(BuiltinScalar::Boolean) => match &value {
            Json::Bool(_) => Ok(value),
            _ => Err(err("a boolean")),
        },
        Some(BuiltinScalar::Id) => match &value {
            Json::String(_) => Ok(value),
            Json::Number(n) if n.is_i64() || n.is_u64() => Ok(Json::String(n.to_string())),
            _ => Err(err("a String or a 32-bit integer")),
        },
        // Custom scalars (database types) are validated by the database.
        None => Ok(value),
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
}
