//! Lowering remote schema root fields to outgoing requests (spec 08 §6.2).

use std::sync::Arc;

use async_graphql_value::Value as GqlValue;
use hge_core::Error;
use hge_ir::{
    NamespaceItem, OperationKind, RemoteCall, RemoteRootField, RemoteRootKind, RootField,
    TypenameRewrite, TypenameTree,
};
use indexmap::IndexSet;
use serde_json::{Map, Value as Json};

use super::print::{self, OutField, OutOperation, OutSelection};
use super::{RemoteField, RemoteSchemaInfo, introspection};
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
