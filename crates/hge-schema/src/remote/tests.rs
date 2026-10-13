use serde_json::json;

use super::metadata::{Customization, FieldNamesCustomization, TypeNamesCustomization};
use super::*;
use crate::model::TypeDef;

fn named(kind: &str, name: &str) -> serde_json::Value {
    json!({"kind": kind, "name": name, "ofType": null})
}

fn field(name: &str, ty: serde_json::Value) -> serde_json::Value {
    json!({"name": name, "description": null, "args": [], "type": ty,
           "isDeprecated": false, "deprecationReason": null})
}

/// `type Query { hello: String, character: Character }`, `interface Character { id: ID! }`,
/// `type Human implements Character { id: ID! }`.
fn introspection() -> serde_json::Value {
    json!({"data": {"__schema": {
        "queryType": {"name": "Query"},
        "mutationType": null,
        "subscriptionType": null,
        "types": [
            {"kind": "OBJECT", "name": "Query", "description": null, "interfaces": [],
             "fields": [field("hello", named("SCALAR", "String")),
                        field("character", named("INTERFACE", "Character"))]},
            {"kind": "INTERFACE", "name": "Character", "description": "A character",
             "fields": [field("id", json!({"kind": "NON_NULL", "name": null, "ofType": named("SCALAR", "ID")}))],
             "possibleTypes": [named("OBJECT", "Human")]},
            {"kind": "OBJECT", "name": "Human", "description": null,
             "interfaces": [named("INTERFACE", "Character")],
             "fields": [field("id", json!({"kind": "NON_NULL", "name": null, "ofType": named("SCALAR", "ID")}))]},
            {"kind": "SCALAR", "name": "String", "description": null},
            {"kind": "SCALAR", "name": "ID", "description": null}
        ],
        "directives": []
    }}})
}

#[test]
fn parses_and_customizes() {
    let c = Customization {
        root_fields_namespace: Some("ns".into()),
        type_names: Some(TypeNamesCustomization {
            prefix: Some("P_".into()),
            ..Default::default()
        }),
        field_names: None,
    };
    let info = RemoteSchemaInfo::new("r", Some(&c), &introspection()).unwrap();
    assert_eq!(info.customized.query_root, "nsQuery");
    assert!(
        matches!(info.customized.get("P_Character"), Some(TypeDef::Interface(i))
        if i.possible_types == ["P_Human"] && i.description.as_deref() == Some("A character"))
    );
    assert_eq!(info.original_type("P_Human"), "Human");
    assert_eq!(
        info.to_custom.get("Query").map(String::as_str),
        Some("nsQuery")
    );
    // Built-in scalars are not prefixed.
    assert!(info.customized.get("String").is_some());
}

#[test]
fn inconsistent_interface_field_customization() {
    let c = Customization {
        field_names: Some(vec![FieldNamesCustomization {
            parent_type: "Human".into(),
            prefix: Some("h_".into()),
            suffix: None,
            mapping: Default::default(),
        }]),
        ..Default::default()
    };
    let err = RemoteSchemaInfo::new("r", Some(&c), &introspection()).unwrap_err();
    assert_eq!(
        err,
        "Remote schema customization inconsistency: field name mapping for field \"id\" of interface \"Character\" is inconsistent with mapping for type \"Human\". Interface field name maps to \"id\". Type field name maps to \"h_id\"."
    );
}

#[test]
fn duplicate_type_mappings() {
    let mut mapping = indexmap::IndexMap::new();
    mapping.insert("Human".to_owned(), "Foo".to_owned());
    mapping.insert("Character".to_owned(), "Foo".to_owned());
    let c = Customization {
        type_names: Some(TypeNamesCustomization {
            mapping,
            ..Default::default()
        }),
        ..Default::default()
    };
    let err = RemoteSchemaInfo::new("r", Some(&c), &introspection()).unwrap_err();
    assert_eq!(
        err,
        "Type name mappings are not distinct; the following types appear more than once: \"Foo\""
    );
}

#[test]
fn introspection_errors() {
    let mut bad = introspection();
    bad["data"]["__schema"]["types"][1]["possibleTypes"][0]["name"] = json!(null);
    assert_eq!(
        RemoteSchemaInfo::new("r", None, &bad).unwrap_err(),
        "Error in $.types[1].possibleTypes[0].name: parsing Name failed, expected String, but encountered Null"
    );
    let mut missing = introspection();
    missing["data"]["__schema"]["types"][0]["fields"][1]["type"] = named("OBJECT", "Nope");
    assert_eq!(
        RemoteSchemaInfo::new("r", None, &missing).unwrap_err(),
        "Could not find type with name \"Nope\""
    );
    let mut empty = introspection();
    empty["data"]["__schema"]["types"][1]["fields"] = json!([]);
    assert!(
        RemoteSchemaInfo::new("r", None, &empty)
            .unwrap_err()
            .starts_with("List of fields cannot be empty for interface 'Character'")
            || RemoteSchemaInfo::new("r", None, &empty)
                .unwrap_err()
                .starts_with("Interface field")
    );
}

#[test]
fn introspection_query_is_valid_graphql() {
    let q = introspection::introspection_query();
    assert!(async_graphql_parser::parse_query(q).is_ok());
    assert!(q.starts_with("query IntrospectionQuery { __schema { queryType { name  } "));
}
