//! GraphQL introspection (`__schema`, `__type`, `__typename`), answered from the schema model.

use indexmap::IndexMap;
use serde_json::{Map, Value as Json};

use crate::model::*;
use crate::normalize::{Selected, SelectedField};

fn intro(name: &str, ty: TypeRef) -> FieldDef {
    FieldDef::new(name, ty, FieldKind::Introspection)
}

fn include_deprecated() -> InputValueDef {
    InputValueDef {
        name: "includeDeprecated".to_owned(),
        description: None,
        ty: TypeRef::named("Boolean"),
        default_value: Some("false".to_owned()),
    }
}

/// The introspection types, declared exactly as V2 declares them. V2's declarations are not the
/// spec's (lists show as plain objects, most scalars as `String!`, no `specifiedByURL` or
/// `__DirectiveLocation`), but the values returned are the spec's.
pub fn add_introspection_types(schema: &mut Schema) {
    let s = |n: &str| TypeRef::named(n);
    let nn = |n: &str| TypeRef::nn(n);

    schema.add(TypeDef::Object(
        ObjectDef::new("__Schema", None)
            .field(intro("description", nn("String")))
            .field(intro("types", s("__Type")))
            .field(intro("queryType", s("__Type")))
            .field(intro("mutationType", s("__Type")))
            .field(intro("subscriptionType", s("__Type")))
            .field(intro("directives", s("__Directive"))),
    ));
    schema.add(TypeDef::Object(
        ObjectDef::new("__Type", None)
            .field(intro("kind", nn("__TypeKind")))
            .field(intro("name", nn("String")))
            .field(intro("description", nn("String")))
            .field(intro("fields", s("__Field")).arg(include_deprecated()))
            .field(intro("interfaces", s("__Type")))
            .field(intro("possibleTypes", s("__Type")))
            .field(intro("enumValues", s("__EnumValue")).arg(include_deprecated()))
            .field(intro("inputFields", s("__InputValue")))
            .field(intro("ofType", s("__Type"))),
    ));
    schema.add(TypeDef::Object(
        ObjectDef::new("__Field", None)
            .field(intro("name", nn("String")))
            .field(intro("description", nn("String")))
            .field(intro("args", s("__InputValue")))
            .field(intro("type", s("__Type")))
            .field(intro("isDeprecated", nn("String")))
            .field(intro("deprecationReason", nn("String"))),
    ));
    schema.add(TypeDef::Object(
        ObjectDef::new("__InputValue", None)
            .field(intro("name", nn("String")))
            .field(intro("description", nn("String")))
            .field(intro("type", s("__Type")))
            .field(intro("defaultValue", nn("String"))),
    ));
    schema.add(TypeDef::Object(
        ObjectDef::new("__EnumValue", None)
            .field(intro("name", nn("String")))
            .field(intro("description", nn("String")))
            .field(intro("isDeprecated", nn("String")))
            .field(intro("deprecationReason", nn("String"))),
    ));
    schema.add(TypeDef::Object(
        ObjectDef::new("__Directive", None)
            .field(intro("name", nn("String")))
            .field(intro("description", nn("String")))
            .field(intro("isRepeatable", nn("Boolean")))
            .field(intro("locations", nn("String")))
            .field(intro("args", s("__InputValue"))),
    ));
    let enum_def = |name: &str, values: &[&str]| {
        TypeDef::Enum(EnumDef {
            name: name.to_owned(),
            description: None,
            values: values
                .iter()
                .map(|v| {
                    (
                        (*v).to_owned(),
                        EnumValueDef {
                            name: (*v).to_owned(),
                            description: None,
                            deprecation_reason: None,
                        },
                    )
                })
                .collect(),
        })
    };
    schema.add(enum_def(
        "__TypeKind",
        &[
            "SCALAR",
            "OBJECT",
            "INTERFACE",
            "UNION",
            "ENUM",
            "INPUT_OBJECT",
            "LIST",
            "NON_NULL",
        ],
    ));
}

/// `@include`, `@skip` and V2's `@cached`.
pub fn standard_directives() -> Vec<DirectiveDef> {
    // V2 gives `if` no description.
    let if_arg = || {
        let mut args = IndexMap::new();
        args.insert(
            "if".to_owned(),
            InputValueDef::new("if", TypeRef::nn("Boolean")),
        );
        args
    };
    let mut cached_args = IndexMap::new();
    cached_args.insert(
        "ttl".to_owned(),
        InputValueDef {
            name: "ttl".to_owned(),
            description: Some("measured in seconds".to_owned()),
            ty: TypeRef::nn("Int"),
            default_value: Some("60".to_owned()),
        },
    );
    cached_args.insert(
        "refresh".to_owned(),
        InputValueDef {
            name: "refresh".to_owned(),
            description: Some("refresh the cache entry".to_owned()),
            ty: TypeRef::nn("Boolean"),
            default_value: Some("false".to_owned()),
        },
    );
    vec![
        DirectiveDef {
            name: "include".to_owned(),
            description: Some("whether this query should be included".to_owned()),
            locations: vec!["FIELD", "FRAGMENT_SPREAD", "INLINE_FRAGMENT"],
            args: if_arg(),
            is_repeatable: false,
        },
        DirectiveDef {
            name: "skip".to_owned(),
            description: Some("whether this query should be skipped".to_owned()),
            locations: vec!["FIELD", "FRAGMENT_SPREAD", "INLINE_FRAGMENT"],
            args: if_arg(),
            is_repeatable: false,
        },
        DirectiveDef {
            name: "cached".to_owned(),
            description: Some("whether this query should be cached (Hasura Cloud only)".to_owned()),
            locations: vec!["QUERY"],
            args: cached_args,
            is_repeatable: false,
        },
    ]
}

/// `__schema` and `__type(name:)`, available on the query root but not listed in it.
pub fn meta_fields() -> IndexMap<String, FieldDef> {
    let mut m = IndexMap::new();
    m.insert(
        "__schema".to_owned(),
        FieldDef::new("__schema", TypeRef::nn("__Schema"), FieldKind::SchemaField),
    );
    m.insert(
        "__type".to_owned(),
        FieldDef::new("__type", TypeRef::named("__Type"), FieldKind::TypeField)
            .arg(InputValueDef::new("name", TypeRef::nn("String"))),
    );
    m
}

/// Evaluate `__schema` or `__type`.
pub fn resolve_root(schema: &Schema, field: &SelectedField<'_>) -> Json {
    let Selected::Field(def) = field.kind else {
        return Json::Null;
    };
    match def.kind {
        FieldKind::SchemaField => schema_object(schema, &field.selection),
        FieldKind::TypeField => {
            let name = field.arg("name").and_then(Json::as_str).unwrap_or("");
            if schema.types.contains_key(name) {
                type_object(schema, &TypeRef::named(name), &field.selection)
            } else {
                Json::Null
            }
        }
        _ => Json::Null,
    }
}

fn select(
    selection: &[SelectedField<'_>],
    typename: &str,
    mut f: impl FnMut(&SelectedField<'_>) -> Json,
) -> Json {
    let mut out = Map::new();
    for sel in selection {
        let v = match sel.kind {
            Selected::Typename(_) => Json::String(typename.to_owned()),
            Selected::Field(_) => f(sel),
        };
        out.insert(sel.alias.clone(), v);
    }
    Json::Object(out)
}

fn schema_object(schema: &Schema, selection: &[SelectedField<'_>]) -> Json {
    select(selection, "__Schema", |sel| match sel.name.as_str() {
        "description" => Json::Null,
        "types" => Json::Array(
            schema
                .types
                .keys()
                .map(|name| type_object(schema, &TypeRef::named(name.clone()), &sel.selection))
                .collect(),
        ),
        "queryType" => type_object(
            schema,
            &TypeRef::named(schema.query_type.clone()),
            &sel.selection,
        ),
        "mutationType" => schema
            .mutation_type
            .as_ref()
            .map(|t| type_object(schema, &TypeRef::named(t.clone()), &sel.selection))
            .unwrap_or(Json::Null),
        "subscriptionType" => schema
            .subscription_type
            .as_ref()
            .map(|t| type_object(schema, &TypeRef::named(t.clone()), &sel.selection))
            .unwrap_or(Json::Null),
        "directives" => Json::Array(
            schema
                .directives
                .iter()
                .map(|d| directive_object(schema, d, &sel.selection))
                .collect(),
        ),
        _ => Json::Null,
    })
}

fn opt_str(s: Option<&str>) -> Json {
    s.map(|s| Json::String(s.to_owned())).unwrap_or(Json::Null)
}

fn include_deprecated_arg(sel: &SelectedField<'_>) -> bool {
    sel.arg("includeDeprecated")
        .and_then(Json::as_bool)
        .unwrap_or(false)
}

fn type_object(schema: &Schema, ty: &TypeRef, selection: &[SelectedField<'_>]) -> Json {
    match ty {
        TypeRef::NonNull(inner) | TypeRef::List(inner) => {
            let kind = if matches!(ty, TypeRef::NonNull(_)) {
                "NON_NULL"
            } else {
                "LIST"
            };
            select(selection, "__Type", |sel| match sel.name.as_str() {
                "kind" => Json::String(kind.to_owned()),
                "ofType" => type_object(schema, inner, &sel.selection),
                _ => Json::Null,
            })
        }
        TypeRef::Named(name) => {
            let Some(def) = schema.get(name) else {
                return Json::Null;
            };
            select(selection, "__Type", |sel| match sel.name.as_str() {
                "kind" => Json::String(def.kind().to_owned()),
                "name" => Json::String(name.clone()),
                "description" => opt_str(def.description()),
                "fields" => match def {
                    TypeDef::Object(obj) => {
                        let with_deprecated = include_deprecated_arg(sel);
                        Json::Array(
                            obj.fields
                                .values()
                                .filter(|f| with_deprecated || f.deprecation_reason.is_none())
                                .map(|f| field_object(schema, f, &sel.selection))
                                .collect(),
                        )
                    }
                    _ => Json::Null,
                },
                "interfaces" => match def {
                    TypeDef::Object(obj) => Json::Array(
                        obj.interfaces
                            .iter()
                            .map(|i| {
                                type_object(schema, &TypeRef::named(i.clone()), &sel.selection)
                            })
                            .collect(),
                    ),
                    _ => Json::Null,
                },
                "possibleTypes" => Json::Null,
                "enumValues" => match def {
                    TypeDef::Enum(e) => {
                        let with_deprecated = include_deprecated_arg(sel);
                        // Values keep definition order (error messages list them that way);
                        // introspection shows them sorted, as V2 does.
                        let mut values: Vec<_> = e
                            .values
                            .values()
                            .filter(|v| with_deprecated || v.deprecation_reason.is_none())
                            .collect();
                        values.sort_by(|a, b| a.name.cmp(&b.name));
                        Json::Array(
                            values
                                .into_iter()
                                .map(|v| enum_value_object(v, &sel.selection))
                                .collect(),
                        )
                    }
                    _ => Json::Null,
                },
                "inputFields" => match def {
                    TypeDef::InputObject(obj) => Json::Array(
                        obj.fields
                            .values()
                            .map(|f| input_value_object(schema, f, &sel.selection))
                            .collect(),
                    ),
                    _ => Json::Null,
                },
                "ofType" => Json::Null,
                _ => Json::Null,
            })
        }
    }
}

fn field_object(schema: &Schema, f: &FieldDef, selection: &[SelectedField<'_>]) -> Json {
    select(selection, "__Field", |sel| match sel.name.as_str() {
        "name" => Json::String(f.name.clone()),
        "description" => opt_str(f.description.as_deref()),
        "args" => Json::Array(
            f.args
                .values()
                .map(|a| input_value_object(schema, a, &sel.selection))
                .collect(),
        ),
        "type" => type_object(schema, &f.ty, &sel.selection),
        "isDeprecated" => Json::Bool(f.deprecation_reason.is_some()),
        "deprecationReason" => opt_str(f.deprecation_reason.as_deref()),
        _ => Json::Null,
    })
}

fn input_value_object(schema: &Schema, v: &InputValueDef, selection: &[SelectedField<'_>]) -> Json {
    select(selection, "__InputValue", |sel| match sel.name.as_str() {
        "name" => Json::String(v.name.clone()),
        "description" => opt_str(v.description.as_deref()),
        "type" => type_object(schema, &v.ty, &sel.selection),
        "defaultValue" => opt_str(v.default_value.as_deref()),
        _ => Json::Null,
    })
}

fn enum_value_object(v: &EnumValueDef, selection: &[SelectedField<'_>]) -> Json {
    select(selection, "__EnumValue", |sel| match sel.name.as_str() {
        "name" => Json::String(v.name.clone()),
        "description" => opt_str(v.description.as_deref()),
        "isDeprecated" => Json::Bool(v.deprecation_reason.is_some()),
        "deprecationReason" => opt_str(v.deprecation_reason.as_deref()),
        _ => Json::Null,
    })
}

fn directive_object(schema: &Schema, d: &DirectiveDef, selection: &[SelectedField<'_>]) -> Json {
    select(selection, "__Directive", |sel| match sel.name.as_str() {
        "name" => Json::String(d.name.clone()),
        "description" => opt_str(d.description.as_deref()),
        "isRepeatable" => Json::Bool(d.is_repeatable),
        "locations" => Json::Array(
            d.locations
                .iter()
                .map(|l| Json::String((*l).to_owned()))
                .collect(),
        ),
        "args" => Json::Array(
            d.args
                .values()
                .map(|a| input_value_object(schema, a, &sel.selection))
                .collect(),
        ),
        _ => Json::Null,
    })
}
