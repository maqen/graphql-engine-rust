//! Action root fields and custom types in a role's schema (V2's `Schema/Action.hs`).

use std::sync::Arc;

use hge_core::RoleName;
use indexmap::IndexMap;

use super::AsyncResultField;
use super::resolve::{ActionInfo, ActionRoot, CustomField, ObjectTypeInfo, ResolvedActions};
use crate::build::Builder;
use crate::model::*;

pub(crate) fn add_root_fields(
    b: &mut Builder<'_>,
    actions: &ResolvedActions,
    role: &RoleName,
    query_root: &mut ObjectDef,
    mutation_root: &mut ObjectDef,
) {
    for action in actions.actions.values() {
        if !action.allowed(role.as_str()) {
            continue;
        }
        let mut field = FieldDef::new(
            action.name.clone(),
            action.output_type.clone(),
            FieldKind::Action(action.clone()),
        )
        .describe_opt(action.description.clone());
        for arg in &action.arguments {
            ensure_type(b, actions, arg.ty.base());
            field = field.arg(input_value(arg));
        }
        match action.root {
            ActionRoot::Query => {
                ensure_type(b, actions, action.output_type.base());
                query_root.fields.insert(field.name.clone(), field);
            }
            ActionRoot::SyncMutation => {
                ensure_type(b, actions, action.output_type.base());
                mutation_root.fields.insert(field.name.clone(), field);
            }
            ActionRoot::AsyncMutation => {
                field.ty = TypeRef::nn("uuid");
                b.scalars.insert("uuid".to_owned());
                mutation_root.fields.insert(field.name.clone(), field);
                let query = async_query_field(b, actions, action);
                query_root.fields.insert(query.name.clone(), query);
            }
        }
    }
}

fn input_value(f: &CustomField) -> InputValueDef {
    InputValueDef::new(f.name.clone(), f.ty.clone()).describe_opt(f.description.clone())
}

/// `<action>(id: uuid!): <action>`, with the `<action>` result object type.
fn async_query_field(
    b: &mut Builder<'_>,
    actions: &ResolvedActions,
    action: &Arc<ActionInfo>,
) -> FieldDef {
    for s in ["uuid", "timestamptz", "json"] {
        b.scalars.insert(s.to_owned());
    }
    ensure_type(b, actions, action.output_type.base());
    // A scalar output has no result object; V2 exposes the scalar itself (quirk Q7).
    let ty = if action.output_object.is_some() {
        let result = ObjectDef::new(
            action.name.clone(),
            Some(format!("fields of action: \"{}\"", action.name)),
        )
        .field(
            FieldDef::new(
                "id",
                TypeRef::nn("uuid"),
                FieldKind::AsyncActionResult(AsyncResultField::Id),
            )
            .describe("the unique id of an action"),
        )
        .field(
            FieldDef::new(
                "created_at",
                TypeRef::nn("timestamptz"),
                FieldKind::AsyncActionResult(AsyncResultField::CreatedAt),
            )
            .describe("the time at which this action was created"),
        )
        .field(
            FieldDef::new(
                "errors",
                TypeRef::named("json"),
                FieldKind::AsyncActionResult(AsyncResultField::Errors),
            )
            .describe("errors related to the invocation"),
        )
        .field(
            FieldDef::new(
                "output",
                action.output_type.clone(),
                FieldKind::AsyncActionResult(AsyncResultField::Output),
            )
            .describe("the output fields of this action"),
        );
        b.schema.add(TypeDef::Object(result));
        TypeRef::named(action.name.clone())
    } else {
        action.output_type.clone()
    };
    FieldDef::new(
        action.name.clone(),
        ty,
        FieldKind::AsyncActionQuery(action.clone()),
    )
    .describe_opt(action.description.clone())
    .arg(InputValueDef::new("id", TypeRef::nn("uuid")).describe("the unique id of an action"))
}

/// Add the named type (and everything it references) to the schema.
fn ensure_type(b: &mut Builder<'_>, actions: &ResolvedActions, name: &str) {
    if b.schema.types.contains_key(name) {
        return;
    }
    let types = &actions.types;
    if let Some(obj) = types.objects.get(name) {
        ensure_object(b, actions, obj);
    } else if let Some(io) = types.input_objects.get(name) {
        let mut def = InputObjectDef {
            name: io.name.clone(),
            description: io.description.clone(),
            fields: IndexMap::new(),
        };
        for f in &io.fields {
            def.fields.insert(f.name.clone(), input_value(f));
        }
        b.schema.add(TypeDef::InputObject(def));
        for f in &io.fields {
            ensure_type(b, actions, f.ty.base());
        }
    } else if let Some(e) = types.enums.get(name) {
        b.schema.add(TypeDef::Enum(EnumDef {
            name: e.name.clone(),
            description: e.description.clone(),
            values: e
                .values
                .iter()
                .map(|(v, d)| {
                    (
                        v.clone(),
                        EnumValueDef {
                            name: v.clone(),
                            description: d.clone(),
                            deprecation_reason: None,
                        },
                    )
                })
                .collect(),
        }));
    } else if let Some(description) = types.scalars.get(name) {
        b.schema.add(TypeDef::Scalar(ScalarDef {
            name: name.to_owned(),
            description: description.clone(),
        }));
    } else {
        // Built-in or Postgres scalar.
        b.scalars.insert(name.to_owned());
    }
}

fn ensure_object(b: &mut Builder<'_>, actions: &ResolvedActions, obj: &Arc<ObjectTypeInfo>) {
    // Reserve the name first so recursive types terminate.
    b.schema.add(TypeDef::Object(ObjectDef::new(
        obj.name.clone(),
        obj.description.clone(),
    )));
    let mut def = ObjectDef::new(obj.name.clone(), obj.description.clone());
    for f in &obj.fields {
        ensure_type(b, actions, f.ty.base());
        def.fields.insert(
            f.name.clone(),
            FieldDef::new(f.name.clone(), f.ty.clone(), FieldKind::CustomObjectField)
                .describe_opt(f.description.clone()),
        );
    }
    for rel in &obj.relationships {
        let Some(target) = b.tables.get(&rel.source, &rel.table).cloned() else {
            continue;
        };
        // The relationship exists only for roles that can select from the table.
        if b.access(&target).is_none_or(|a| a.columns.is_empty()) {
            continue;
        }
        b.ensure_table_types(&target);
        let kind = FieldKind::ActionRelationship(rel.clone());
        let field = if rel.array {
            let f = FieldDef::new(
                rel.name.clone(),
                TypeRef::nn_list_nn(target.type_name()),
                kind,
            )
            .describe("An array relationship");
            b.with_select_args(f, &target)
        } else {
            FieldDef::new(rel.name.clone(), TypeRef::named(target.type_name()), kind)
                .describe("An object relationship")
        };
        def.fields.insert(field.name.clone(), field);
    }
    b.schema
        .types
        .insert(def.name.clone(), TypeDef::Object(def));
}
