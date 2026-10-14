//! Lowering remote schema root fields to outgoing requests (spec 08 §6.2).

use std::sync::Arc;

use async_graphql_value::{Name, Value as GqlValue};
use hge_core::Error;
use hge_ir::{
    NamespaceItem, OperationKind, RemoteCall, RemoteJoinTarget, RemoteRootField, RemoteRootKind,
    RootField, TypenameRewrite, TypenameTree,
};
use indexmap::IndexSet;
use serde_json::{Map, Value as Json};

use super::print::{self, OutField, OutOperation, OutSelection};
use super::{RemoteField, RemoteRelationship, RemoteSchemaInfo, introspection};
use crate::model::{FieldKind, TypeDef, TypeRef};
use crate::normalize::{NormalizedOperation, Selected, SelectedField};

/// Lower a root field whose kind is [`FieldKind::Remote`].
pub fn lower_root(
    op: &NormalizedOperation<'_>,
    field: &SelectedField<'_>,
    remote_field: &RemoteField,
) -> Result<RootField, Error> {
    let (remote, kind) = match remote_field {
        RemoteField::Root { remote, original } => (
            remote,
            RemoteRootKind::Field(call(op, remote, field, original)?),
        ),
        RemoteField::Namespace(remote) => {
            let mut items = Vec::with_capacity(field.selection.len());
            for sub in &field.selection {
                let item = match sub.kind {
                    Selected::Typename(t) => NamespaceItem::Static(Json::String(t.to_owned())),
                    Selected::Field(def) => match &def.kind {
                        FieldKind::Remote(RemoteField::Field { original }) => {
                            NamespaceItem::Call(call(op, remote, sub, original)?)
                        }
                        other => return Err(unexpected_kind(other)),
                    },
                };
                items.push((sub.alias.clone(), item));
            }
            (remote, RemoteRootKind::Namespace(items))
        }
        RemoteField::Field { .. } => {
            return Err(Error::unexpected("remote field lowered as a root field"));
        }
    };
    Ok(RootField::Remote(Box::new(RemoteRootField {
        remote_schema: remote.name.clone(),
        kind,
    })))
}

fn unexpected_kind(kind: &FieldKind) -> Error {
    Error::unexpected(format!(
        "unexpected field kind {kind:?} in a remote selection"
    ))
}

fn call(
    op: &NormalizedOperation<'_>,
    remote: &Arc<RemoteSchemaInfo>,
    field: &SelectedField<'_>,
    original: &str,
) -> Result<RemoteCall, Error> {
    let mut cx = Lowerer {
        remote,
        variables: IndexSet::new(),
    };
    let root = cx.field(field, original)?;
    let mut var_defs = Vec::with_capacity(cx.variables.len());
    let mut values = Map::new();
    for name in &cx.variables {
        let (ty, value, _) = op.variables.get(name).ok_or_else(|| {
            Error::validation(format!("unbound variable '{name}'")).with_path(field.path.clone())
        })?;
        var_defs.push((name.clone(), original_type_ref(remote, ty)));
        values.insert(name.clone(), value.clone().unwrap_or(Json::Null));
    }
    let query = print::operation(&OutOperation {
        kind: match op.kind {
            OperationKind::Mutation => "mutation",
            OperationKind::Subscription => "subscription",
            OperationKind::Query => "query",
        },
        name: op.operation_name.as_deref(),
        variables: var_defs,
        selection: vec![OutSelection::Field(root)],
    });
    let typenames = (!remote.to_custom.is_empty()).then(|| TypenameRewrite {
        map: remote.to_custom.clone(),
        tree: typename_tree(field),
    });
    Ok(RemoteCall {
        query,
        operation_name: op.operation_name.clone(),
        variables: (!values.is_empty()).then_some(values),
        response_key: field.alias.clone(),
        typenames: typenames.filter(|t| !t.tree.is_empty()),
    })
}

fn original_type_ref(remote: &RemoteSchemaInfo, ty: &TypeRef) -> TypeRef {
    match ty {
        TypeRef::Named(n) => TypeRef::Named(remote.original_type(n).to_owned()),
        TypeRef::List(t) => TypeRef::List(Box::new(original_type_ref(remote, t))),
        TypeRef::NonNull(t) => TypeRef::NonNull(Box::new(original_type_ref(remote, t))),
    }
}

struct Lowerer<'a> {
    remote: &'a RemoteSchemaInfo,
    /// Variables referenced, in order of first use.
    variables: IndexSet<String>,
}

impl Lowerer<'_> {
    fn field(&mut self, sel: &SelectedField<'_>, original: &str) -> Result<OutField, Error> {
        let Selected::Field(def) = sel.kind else {
            return Ok(OutField::new("__typename").with_alias(&sel.alias));
        };
        let mut out = OutField::new(original).with_alias(&sel.alias);
        for (name, value) in &sel.raw_args {
            self.note_variables(value);
            out.args.push((name.clone(), value.clone()));
        }
        // Omitted arguments with a default are sent with their default.
        for (name, arg) in &def.args {
            if sel.raw_args.iter().any(|(n, _)| n == name) {
                continue;
            }
            if let Some(default) = arg
                .default_value
                .as_deref()
                .and_then(introspection::parse_const_value)
            {
                out.args.push((name.clone(), default.into_value()));
            }
        }
        out.selection = if sel.by_type.is_empty() {
            self.selection(&sel.selection)?
        } else {
            self.abstract_selection(def.ty.base(), &sel.by_type)?
        };
        Ok(out)
    }

    fn selection(&mut self, fields: &[SelectedField<'_>]) -> Result<Vec<OutSelection>, Error> {
        fields
            .iter()
            .map(|f| self.child(f).map(OutSelection::Field))
            .collect()
    }

    fn child(&mut self, f: &SelectedField<'_>) -> Result<OutField, Error> {
        match f.kind {
            Selected::Typename(_) => Ok(OutField::new("__typename").with_alias(&f.alias)),
            Selected::Field(def) => match &def.kind {
                FieldKind::Remote(RemoteField::Field { original }) => self.field(f, original),
                other => Err(unexpected_kind(other)),
            },
        }
    }

    /// V2's `reduceAbstractTypeSelectionSet`: the fields every member selects first and the
    /// abstract type defines become the base selection; the rest goes into one inline
    /// fragment per member.
    fn abstract_selection(
        &mut self,
        abstract_type: &str,
        by_type: &[(&str, Vec<SelectedField<'_>>)],
    ) -> Result<Vec<OutSelection>, Error> {
        let interface_fields = match self.remote.customized.get(abstract_type) {
            Some(TypeDef::Interface(i)) => Some(&i.fields),
            _ => None,
        };
        let mut members = Vec::with_capacity(by_type.len());
        for (ty, fields) in by_type {
            let mut out = Vec::with_capacity(fields.len());
            for f in fields {
                let on_abstract = match f.kind {
                    Selected::Typename(_) => true,
                    Selected::Field(_) => interface_fields.is_some_and(|i| i.contains_key(&f.name)),
                };
                out.push((OutSelection::Field(self.child(f)?), on_abstract));
            }
            members.push((*ty, out));
        }
        let Some((_, first)) = members.first() else {
            return Ok(vec![]);
        };
        let mut base_len = 0;
        while base_len < first.len()
            && first[base_len].1
            && members
                .iter()
                .all(|(_, m)| m.get(base_len).is_some_and(|s| s.0 == first[base_len].0))
        {
            base_len += 1;
        }
        let mut out: Vec<OutSelection> = first[..base_len].iter().map(|s| s.0.clone()).collect();
        for (ty, sels) in members {
            if sels.len() > base_len {
                out.push(OutSelection::InlineFragment {
                    on: self.remote.original_type(ty).to_owned(),
                    selection: sels.into_iter().skip(base_len).map(|s| s.0).collect(),
                });
            }
        }
        Ok(out)
    }

    fn note_variables(&mut self, value: &GqlValue) {
        match value {
            GqlValue::Variable(n) => {
                self.variables.insert(n.to_string());
            }
            GqlValue::List(items) => items.iter().for_each(|v| self.note_variables(v)),
            GqlValue::Object(fields) => fields.values().for_each(|v| self.note_variables(v)),
            _ => {}
        }
    }
}

/// Where `__typename` values appear below `field`.
fn typename_tree(field: &SelectedField<'_>) -> TypenameTree {
    let mut tree = TypenameTree::default();
    let add = |fields: &[SelectedField<'_>], tree: &mut TypenameTree| {
        for f in fields {
            match f.kind {
                Selected::Typename(_) => {
                    if !tree.typename_keys.contains(&f.alias) {
                        tree.typename_keys.push(f.alias.clone());
                    }
                }
                Selected::Field(_) => {
                    let child = typename_tree(f);
                    if child.is_empty() {
                        continue;
                    }
                    match tree.children.iter_mut().find(|(k, _)| *k == f.alias) {
                        Some((_, existing)) => merge(existing, child),
                        None => tree.children.push((f.alias.clone(), child)),
                    }
                }
            }
        }
    };
    add(&field.selection, &mut tree);
    for (_, fields) in &field.by_type {
        add(fields, &mut tree);
    }
    tree
}

fn merge(into: &mut TypenameTree, other: TypenameTree) {
    for k in other.typename_keys {
        if !into.typename_keys.contains(&k) {
            into.typename_keys.push(k);
        }
    }
    for (k, child) in other.children {
        match into.children.iter_mut().find(|(key, _)| *key == k) {
            Some((_, existing)) => merge(existing, child),
            None => into.children.push((k, child)),
        }
    }
}

/// The join request of a remote relationship field (spec 08 §9.4): the user's arguments
/// and selection are fixed here; the join values come with each request.
pub fn join_target(
    variables: Option<&crate::normalize::Variables>,
    rel: &Arc<RemoteRelationship>,
    field: &SelectedField<'_>,
) -> Result<Arc<dyn RemoteJoinTarget>, Error> {
    let leaf = rel
        .hops
        .last()
        .ok_or_else(|| Error::unexpected("remote relationship without a remote field"))?;
    let value_of = |name: &str| -> Result<Json, Error> {
        let (_, value, _) = variables.and_then(|v| v.get(name)).ok_or_else(|| {
            Error::validation(format!("unbound variable '{name}'")).with_path(field.path.clone())
        })?;
        Ok(value.clone().unwrap_or(Json::Null))
    };
    let mut cx = Lowerer {
        remote: &rel.remote,
        variables: IndexSet::new(),
    };
    let mut user_args = Vec::with_capacity(field.raw_args.len());
    for (name, value) in &field.raw_args {
        // An argument merged with the template is sent as a literal.
        let value = if leaf.arguments.contains_key(name) {
            inline_variables(value, &value_of)?
        } else {
            cx.note_variables(value);
            value.clone()
        };
        user_args.push((name.clone(), value));
    }
    // Omitted arguments with a default are sent with their default, as for root fields.
    for (name, arg) in &rel.args {
        if field.raw_args.iter().any(|(n, _)| n == name) || leaf.arguments.contains_key(name) {
            continue;
        }
        if let Some(default) = arg
            .default_value
            .as_deref()
            .and_then(introspection::parse_const_value)
        {
            user_args.push((name.clone(), default.into_value()));
        }
    }
    let selection = if field.by_type.is_empty() {
        cx.selection(&field.selection)?
    } else {
        cx.abstract_selection(rel.ty.base(), &field.by_type)?
    };
    let mut var_defs = Vec::with_capacity(cx.variables.len());
    let mut values = Map::new();
    for name in &cx.variables {
        let (ty, _, _) = variables.and_then(|v| v.get(name)).ok_or_else(|| {
            Error::validation(format!("unbound variable '{name}'")).with_path(field.path.clone())
        })?;
        var_defs.push((name.clone(), map_type_ref(ty, &|n| rel.original_type(n))));
        values.insert(name.clone(), value_of(name)?);
    }
    let typenames = (!rel.remote.to_custom.is_empty())
        .then(|| TypenameRewrite {
            map: rel.remote.to_custom.clone(),
            tree: typename_tree(field),
        })
        .filter(|t| !t.tree.is_empty());
    Ok(Arc::new(JoinTarget {
        rel: rel.clone(),
        user_args,
        selection,
        var_defs,
        values,
        typenames,
    }))
}

fn map_type_ref<'a>(ty: &'a TypeRef, f: &dyn Fn(&'a str) -> &'a str) -> TypeRef {
    match ty {
        TypeRef::Named(n) => TypeRef::Named(f(n).to_owned()),
        TypeRef::List(t) => TypeRef::List(Box::new(map_type_ref(t, f))),
        TypeRef::NonNull(t) => TypeRef::NonNull(Box::new(map_type_ref(t, f))),
    }
}

/// Replace the variables in `v` by their values.
fn inline_variables(
    v: &GqlValue,
    value_of: &dyn Fn(&str) -> Result<Json, Error>,
) -> Result<GqlValue, Error> {
    Ok(match v {
        GqlValue::Variable(n) => json_to_graphql(&value_of(n)?),
        GqlValue::List(items) => GqlValue::List(
            items
                .iter()
                .map(|i| inline_variables(i, value_of))
                .collect::<Result<_, _>>()?,
        ),
        GqlValue::Object(fields) => GqlValue::Object(
            fields
                .iter()
                .map(|(k, i)| Ok((k.clone(), inline_variables(i, value_of)?)))
                .collect::<Result<_, Error>>()?,
        ),
        other => other.clone(),
    })
}

/// V2's `jsonToGraphQL`: join values are sent as literals.
fn json_to_graphql(v: &Json) -> GqlValue {
    match v {
        Json::Null => GqlValue::Null,
        Json::Bool(b) => GqlValue::Boolean(*b),
        Json::Number(n) => GqlValue::Number(n.clone()),
        Json::String(s) => GqlValue::String(s.clone()),
        Json::Array(items) => GqlValue::List(items.iter().map(json_to_graphql).collect()),
        Json::Object(o) => GqlValue::Object(
            o.iter()
                .map(|(k, v)| (Name::new(k), json_to_graphql(v)))
                .collect(),
        ),
    }
}

#[derive(Debug)]
struct JoinTarget {
    rel: Arc<RemoteRelationship>,
    /// The user's arguments on the leaf field (variables inlined where they meet the
    /// template).
    user_args: Vec<(String, GqlValue)>,
    selection: Vec<OutSelection>,
    var_defs: Vec<(String, TypeRef)>,
    values: Map<String, Json>,
    typenames: Option<TypenameRewrite>,
}

impl JoinTarget {
    /// A template value with the join values of `argument` substituted.
    fn template(v: &Json, argument: &Map<String, Json>) -> Result<GqlValue, Error> {
        Ok(match v {
            Json::String(s) if s.starts_with('$') => {
                let name = &s[1..];
                let value = argument.get(name).ok_or_else(|| {
                    Error::unexpected(format!(
                        "Found errors: Value for variable \"{name}\" not provided"
                    ))
                    .with_status(400)
                })?;
                json_to_graphql(value)
            }
            Json::Array(items) => GqlValue::List(
                items
                    .iter()
                    .map(|i| Self::template(i, argument))
                    .collect::<Result<_, _>>()?,
            ),
            Json::Object(o) => GqlValue::Object(
                o.iter()
                    .map(|(k, i)| Ok((Name::new(k), Self::template(i, argument)?)))
                    .collect::<Result<_, Error>>()?,
            ),
            other => json_to_graphql(other),
        })
    }

    /// `f<id>: hop(args) { … leaf(args) { selection } }`
    fn call_field(&self, id: usize, argument: &Map<String, Json>) -> Result<OutField, Error> {
        let mut inner: Option<OutField> = None;
        let last = self.rel.hops.len() - 1;
        for (i, hop) in self.rel.hops.iter().enumerate().rev() {
            let mut out = OutField::new(hop.field.clone());
            for (name, v) in &hop.arguments {
                out.args.push((name.clone(), Self::template(v, argument)?));
            }
            if i == last {
                for (name, user) in &self.user_args {
                    match out.args.iter_mut().find(|(n, _)| n == name) {
                        Some((_, templated)) => {
                            let t = std::mem::replace(templated, GqlValue::Null);
                            *templated = combine(user.clone(), t, &hop.field)?;
                        }
                        None => out.args.push((name.clone(), user.clone())),
                    }
                }
                out.selection = self.selection.clone();
            } else if let Some(child) = inner.take() {
                out.selection = vec![OutSelection::Field(child)];
            }
            inner = Some(out);
        }
        let mut root =
            inner.ok_or_else(|| Error::unexpected("remote relationship without a field"))?;
        root.alias = Some(format!("f{id}"));
        Ok(root)
    }
}

/// V2's `combineValues`: objects merge key by key, lists concatenate (the user's first).
fn combine(user: GqlValue, template: GqlValue, field: &str) -> Result<GqlValue, Error> {
    match (user, template) {
        (GqlValue::Object(mut a), GqlValue::Object(b)) => {
            for (k, v) in b {
                let merged = match a.shift_remove(&k) {
                    Some(existing) => combine(existing, v, field)?,
                    None => v,
                };
                a.insert(k, merged);
            }
            Ok(GqlValue::Object(a))
        }
        (GqlValue::List(mut a), GqlValue::List(b)) => {
            a.extend(b);
            Ok(GqlValue::List(a))
        }
        (a, b) => {
            let (mut x, mut y) = (String::new(), String::new());
            print::value(&mut x, &a);
            print::value(&mut y, &b);
            Err(Error::unexpected(format!(
                "combineValues: cannot combine values ({x}) and ({y}) for field {field}; lists can only be merged with lists, objects can only be merged with objects"
            ))
            .with_status(500))
        }
    }
}

fn lookup_error(key: &str) -> Error {
    Error::unexpected(format!("failed to lookup key '{key}' in response")).with_status(500)
}

impl RemoteJoinTarget for JoinTarget {
    fn remote_schema(&self) -> &str {
        &self.rel.remote.name
    }

    fn request(&self, arguments: &[(usize, &Map<String, Json>)]) -> Result<RemoteCall, Error> {
        let selection = arguments
            .iter()
            .map(|(id, a)| self.call_field(*id, a).map(OutSelection::Field))
            .collect::<Result<Vec<_>, _>>()?;
        let query = print::operation(&OutOperation {
            kind: "query",
            name: None,
            variables: self.var_defs.clone(),
            selection,
        });
        Ok(RemoteCall {
            query,
            operation_name: None,
            variables: (!self.values.is_empty()).then(|| self.values.clone()),
            response_key: String::new(),
            typenames: None,
        })
    }

    fn extract(&self, data: &Map<String, Json>, id: usize) -> Result<Json, Error> {
        let key = format!("f{id}");
        let mut value = data.get(&key).ok_or_else(|| lookup_error(&key))?;
        for hop in &self.rel.hops[1..] {
            let Json::Object(obj) = value else {
                return Err(Error::unexpected(format!(
                    "unexpected non-object json value found while path not empty: {value}"
                ))
                .with_status(500));
            };
            value = obj
                .get(&hop.field)
                .ok_or_else(|| lookup_error(&hop.field))?;
        }
        let mut out = value.clone();
        if let Some(t) = &self.typenames {
            t.tree.apply(&mut out, &t.map);
        }
        Ok(out)
    }
}
