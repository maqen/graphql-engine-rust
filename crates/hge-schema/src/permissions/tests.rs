use std::collections::HashMap;

use hge_core::{
    ColumnInfo, ColumnType, ForeignKey, QualifiedTable, RoleName, SourceCatalog, TableCatalog,
    TableKind, TableMutability,
};
use hge_ir::{BoolExp, ColumnCompareOp, ColumnRef, CompareOp, RootFields, Value};
use indexmap::IndexMap;
use serde_json::json;

use super::table_permissions;
use crate::Tables;

fn column(name: &str, ty: &str, nullable: bool) -> (String, ColumnInfo) {
    (
        name.to_owned(),
        ColumnInfo {
            name: name.to_owned(),
            ty: ColumnType::builtin(ty),
            nullable,
            has_default: false,
            is_identity: false,
            is_identity_always: false,
            is_generated: false,
            description: None,
        },
    )
}

fn catalog() -> SourceCatalog {
    let author = QualifiedTable::new("public", "author");
    let article = QualifiedTable::new("public", "article");
    let mut tables = HashMap::new();
    tables.insert(
        author.clone(),
        TableCatalog {
            name: author.clone(),
            kind: TableKind::Table,
            description: None,
            columns: IndexMap::from([
                column("id", "int4", false),
                column("name", "text", false),
                column("manager_id", "int4", true),
            ]),
            primary_key: Some(vec!["id".into()]),
            unique_constraints: vec![],
            constraints: vec![],
            mutability: TableMutability::ALL,
            foreign_keys: vec![],
        },
    );
    tables.insert(
        article.clone(),
        TableCatalog {
            name: article.clone(),
            kind: TableKind::Table,
            description: None,
            columns: IndexMap::from([
                column("id", "int4", false),
                column("author_id", "int4", false),
                column("title", "text", false),
                column("data", "json", true),
            ]),
            primary_key: Some(vec!["id".into()]),
            unique_constraints: vec![],
            constraints: vec![],
            mutability: TableMutability::ALL,
            foreign_keys: vec![ForeignKey {
                constraint: "article_author_id_fkey".into(),
                ref_table: author,
                columns: vec![("author_id".into(), "id".into())],
            }],
        },
    );
    SourceCatalog {
        tables,
        enums: HashMap::new(),
    }
}

/// Resolve metadata with the given `author` and `article` table entries (merged into the
/// defaults).
fn resolve(author: serde_json::Value, article: serde_json::Value) -> (Tables, Vec<String>) {
    let mut author_entry = json!({
        "table": {"schema": "public", "name": "author"},
        "array_relationships": [{"name": "articles", "using": {"foreign_key_constraint_on":
            {"column": "author_id", "table": {"schema": "public", "name": "article"}}}}]
    });
    let mut article_entry = json!({
        "table": {"schema": "public", "name": "article"},
        "object_relationships": [{"name": "author", "using": {"foreign_key_constraint_on": "author_id"}}]
    });
    for (entry, extra) in [(&mut author_entry, author), (&mut article_entry, article)] {
        for (k, v) in extra.as_object().unwrap() {
            entry[k] = v.clone();
        }
    }
    let metadata = hge_metadata::from_json(json!({
        "version": 3,
        "sources": [{
            "name": "default",
            "kind": "postgres",
            "configuration": {"connection_info": {"database_url": "postgres://x"}},
            "tables": [author_entry, article_entry]
        }]
    }))
    .unwrap();
    let catalogs = HashMap::from([("default".to_owned(), catalog())]);
    crate::resolve_tables(&metadata, &catalogs)
}

fn author(tables: &Tables) -> &crate::TableInfo {
    tables
        .get("default", &QualifiedTable::new("public", "author"))
        .unwrap()
}

fn article(tables: &Tables) -> &crate::TableInfo {
    tables
        .get("default", &QualifiedTable::new("public", "article"))
        .unwrap()
}

fn select_filter(perm: serde_json::Value) -> Result<BoolExp, Vec<String>> {
    let (tables, errs) = resolve(
        json!({"select_permissions": [{"role": "user", "permission": {"columns": "*", "filter": perm}}]}),
        json!({}),
    );
    match table_permissions(author(&tables), &RoleName::new("user")) {
        Some(p) => Ok(p.select.clone().unwrap().filter),
        None => Err(errs),
    }
}

fn session(name: &str) -> Value {
    Value::Session {
        name: name.into(),
        ty: ColumnType::builtin("int4"),
    }
}

#[test]
fn admin_gets_everything() {
    let (tables, errs) = resolve(json!({}), json!({}));
    assert!(errs.is_empty(), "{errs:?}");
    let p = table_permissions(author(&tables), &RoleName::admin()).unwrap();
    let select = p.select.as_ref().unwrap();
    assert_eq!(select.columns, ["id", "name", "manager_id"]);
    assert!(select.filter.is_true() && select.allow_aggregations && select.limit.is_none());
    assert!(p.insert.is_some() && p.update.is_some() && p.delete.is_some());
    assert!(table_permissions(author(&tables), &RoleName::new("user")).is_none());
}

#[test]
fn session_variables_and_shorthand() {
    // Shorthand is null-safe equality; session variables are case-insensitive.
    assert_eq!(
        select_filter(json!({"id": "X-Hasura-User-Id"})).unwrap(),
        BoolExp::Compare {
            column: "id".into(),
            ty: ColumnType::builtin("int4"),
            op: CompareOp::Eq(session("x-hasura-user-id")),
        }
    );
    assert_eq!(
        select_filter(json!({"id": {"$eq": "req_user_id"}})).unwrap(),
        BoolExp::Compare {
            column: "id".into(),
            ty: ColumnType::builtin("int4"),
            op: CompareOp::Eq(session("x-hasura-user-id")),
        }
    );
    // `_in` with a session variable takes the array type.
    assert_eq!(
        select_filter(json!({"id": {"_in": "x-hasura-ids"}})).unwrap(),
        BoolExp::Compare {
            column: "id".into(),
            ty: ColumnType::builtin("int4"),
            op: CompareOp::In(Value::Session {
                name: "x-hasura-ids".into(),
                ty: ColumnType::builtin("int4").array_of(),
            }),
        }
    );
}

#[test]
fn null_safe_equality() {
    let cmp = |op| BoolExp::Compare {
        column: "manager_id".into(),
        ty: ColumnType::builtin("int4"),
        op,
    };
    assert_eq!(
        select_filter(json!({"manager_id": {"_eq": null}})).unwrap(),
        cmp(CompareOp::IsNull(true))
    );
    assert_eq!(
        select_filter(json!({"manager_id": {"_neq": null}})).unwrap(),
        cmp(CompareOp::IsNull(false))
    );
    assert_eq!(
        select_filter(json!({"manager_id": null})).unwrap(),
        cmp(CompareOp::IsNull(true))
    );
}

#[test]
fn boolean_structure() {
    assert!(select_filter(json!({})).unwrap().is_true());
    assert_eq!(
        select_filter(json!({"_or": []})).unwrap(),
        BoolExp::always_false()
    );
    assert_eq!(
        select_filter(json!({"$not": {}})).unwrap(),
        BoolExp::Not(Box::new(BoolExp::always_true()))
    );
    let rel = select_filter(json!({"articles": {"title": {"_ilike": "%x%"}}})).unwrap();
    assert!(
        matches!(rel, BoolExp::Relationship { ref table, ref mapping, .. }
        if table.name == "article" && mapping == &[("id".to_owned(), "author_id".to_owned())])
    );
    let exists = select_filter(json!({"_exists": {"_table": "article", "_where": {"id": 1}}}));
    assert!(matches!(exists.unwrap(), BoolExp::Exists { table, .. } if table.name == "article"));
}

#[test]
fn column_comparisons() {
    assert_eq!(
        select_filter(json!({"manager_id": {"_cneq": "id"}})).unwrap(),
        BoolExp::CompareColumns {
            column: "manager_id".into(),
            op: ColumnCompareOp::Neq,
            other: ColumnRef::Current("id".into()),
        }
    );
    // `["$", col]` inside a relationship refers to the permission's own table.
    let exp = select_filter(json!({"articles": {"author_id": {"_ceq": ["$", "id"]}}})).unwrap();
    let BoolExp::Relationship { exp, .. } = exp else {
        panic!("expected a relationship")
    };
    assert_eq!(
        *exp,
        BoolExp::CompareColumns {
            column: "author_id".into(),
            op: ColumnCompareOp::Eq,
            other: ColumnRef::Root("id".into()),
        }
    );
    assert!(select_filter(json!({"id": {"_ceq": "name"}})).is_err());
}

#[test]
fn invalid_permissions_are_inconsistent() {
    for (filter, message) in [
        (
            json!({"nope": {"_eq": 1}}),
            "no such column exists: \"nope\"",
        ),
        (json!({"id": {"_foo": 1}}), "Unknown operator: _foo"),
        (json!({"id": {"eq": 1}}), "Unknown operator: eq"),
        (
            json!({"name": {"_has_key": "a"}}),
            "only supported on jsonb",
        ),
        (json!({"id": {"_like": "1"}}), "only supported on string"),
        (
            json!({"id": {"_ceq": ["x", "id"]}}),
            "Relationship references",
        ),
        (
            json!({"_exists": {"_table": "missing", "_where": {}}}),
            "not tracked",
        ),
    ] {
        let errs = select_filter(filter.clone()).unwrap_err();
        assert!(
            errs.len() == 1 && errs[0].contains(message),
            "{filter}: {errs:?}"
        );
    }
    let (tables, errs) = resolve(
        json!({}),
        json!({"select_permissions": [{"role": "user", "permission": {"columns": "*", "filter": {"data": {"_eq": "{}"}}}}]}),
    );
    assert!(table_permissions(article(&tables), &RoleName::new("user")).is_none());
    assert!(errs[0].contains("JSON column can not be part of boolean expression"));
}

#[test]
fn admin_permissions_in_metadata_are_inconsistent() {
    let (tables, errs) = resolve(
        json!({"select_permissions": [{"role": "admin", "permission": {"columns": [], "filter": {}}}]}),
        json!({}),
    );
    assert!(errs[0].contains("cannot define permission for admin role"));
    let admin = table_permissions(author(&tables), &RoleName::admin()).unwrap();
    assert_eq!(admin.select.as_ref().unwrap().columns.len(), 3);
}

#[test]
fn select_columns_and_root_fields() {
    let (tables, errs) = resolve(
        json!({"select_permissions": [
            {"role": "a", "permission": {"columns": ["name", "id"], "filter": {}, "limit": 5,
                "query_root_fields": ["select_by_pk"], "subscription_root_fields": ["select_stream"]}},
            {"role": "b", "permission": {"columns": ["name"], "filter": {},
                "query_root_fields": ["select_by_pk"]}},
            {"role": "c", "permission": {"columns": "*", "filter": {},
                "query_root_fields": ["select_aggregate"]}},
            {"role": "d", "permission": {"columns": ["articles"], "filter": {}}},
        ]}),
        json!({}),
    );
    let a = table_permissions(author(&tables), &RoleName::new("a"))
        .unwrap()
        .select
        .clone()
        .unwrap();
    assert_eq!(a.columns, ["id", "name"]);
    assert_eq!(a.limit, Some(5));
    assert_eq!(
        a.query_root_fields,
        RootFields {
            select_by_pk: true,
            ..RootFields::NONE
        }
    );
    assert_eq!(
        a.subscription_root_fields,
        RootFields {
            select_stream: true,
            ..RootFields::NONE
        }
    );
    assert_eq!(errs.len(), 3, "{errs:?}");
    assert!(errs[0].contains("does not have access to the primary key"));
    assert!(errs[1].contains("when \"allow_aggregations\" is set to true"));
    assert!(errs[2].contains("is a relationship"));
}

#[test]
fn mutation_permissions() {
    let (tables, errs) = resolve(
        json!({
            "insert_permissions": [
                {"role": "user", "permission": {"check": {"id": "x-hasura-user-id"},
                    "set": {"manager_id": "X-Hasura-User-Id"}, "backend_only": true}},
                {"role": "bad", "permission": {"check": {}, "set": {"nope": 1}}},
            ],
            "update_permissions": [
                {"role": "user", "permission": {"columns": ["name", "manager_id"],
                    "filter": {"id": "x-hasura-user-id"}, "set": {"manager_id": 7}}},
            ],
            "delete_permissions": [
                {"role": "user", "permission": {"filter": {"id": {"_eq": 1}}}},
            ],
        }),
        json!({}),
    );
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(errs[0].contains("column \"nope\" not found in table \"author\""));
    let p = table_permissions(author(&tables), &RoleName::new("user")).unwrap();
    assert!(p.select.is_none());

    // Absent insert columns mean all columns; presets are not insertable.
    let insert = p.insert.as_ref().unwrap();
    assert_eq!(insert.columns, ["id", "name"]);
    assert!(insert.backend_only);
    assert_eq!(insert.presets["manager_id"], session("x-hasura-user-id"));

    let update = p.update.as_ref().unwrap();
    assert_eq!(update.columns, ["name"]);
    assert!(update.check.is_none());
    assert_eq!(
        update.presets["manager_id"],
        Value::literal("7", ColumnType::builtin("int4"))
    );

    assert!(p.delete.as_ref().is_some_and(|d| !d.filter.is_true()));
}

#[test]
fn duplicate_permissions_keep_the_first() {
    let (tables, errs) = resolve(
        json!({"select_permissions": [
            {"role": "user", "permission": {"columns": ["id"], "filter": {}}},
            {"role": "user", "permission": {"columns": "*", "filter": {}}},
        ]}),
        json!({}),
    );
    assert_eq!(errs.len(), 1, "{errs:?}");
    let select = table_permissions(author(&tables), &RoleName::new("user"))
        .unwrap()
        .select
        .clone()
        .unwrap();
    assert_eq!(select.columns, ["id"]);
}

#[test]
fn non_ascii_literals() {
    let exp = select_filter(json!({"name": {"_eq": "12345678é"}})).unwrap();
    assert!(matches!(
        exp,
        BoolExp::Compare {
            op: CompareOp::Eq(Value::Literal { .. }),
            ..
        }
    ));
}
