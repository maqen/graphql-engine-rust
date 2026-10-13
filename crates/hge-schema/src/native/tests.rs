use std::collections::HashMap;

use hge_core::{NamedType, ResultColumn};
use serde_json::json;

use super::*;

fn source() -> Source {
    serde_json::from_value(json!({
        "name": "default",
        "kind": "postgres",
        "configuration": {},
        "logical_models": [{
            "name": "summary",
            "description": "A summary",
            "fields": [
                {"name": "id", "type": {"scalar": "int4", "nullable": false}},
                {"name": "title", "type": {"scalar": "text", "nullable": true}, "description": "The title"},
                {"name": "nested", "type": {"logical_model": "other", "nullable": true}}
            ],
            "select_permissions": [{"role": "user", "permission": {"columns": ["id"], "filter": {}}}]
        }],
        "native_queries": [
            {"root_field_name": "ok", "code": "SELECT {{b}} AS id, {{ a }} AS title, {{b}} AS x",
             "arguments": {"a": {"type": "text", "nullable": true}, "b": {"type": "int4"}},
             "returns": "summary", "description": "Fine"},
            {"root_field_name": "no_model", "code": "SELECT 1", "returns": "nope"},
            {"root_field_name": "undeclared", "code": "SELECT {{x}}, {{y}}", "returns": "summary"},
            {"root_field_name": "broken", "code": "SELEC 1", "returns": "summary"},
            {"root_field_name": "mismatch", "code": "SELECT 'x' AS id, 't' AS title", "returns": "summary"}
        ]
    }))
    .unwrap()
}

fn named(name: &str, display: &str, oid: u32) -> NamedType {
    NamedType {
        ty: ColumnType::builtin(name),
        display: display.to_owned(),
        oid,
    }
}

fn column(name: &str, display: &str, oid: u32) -> ResultColumn {
    ResultColumn {
        name: name.to_owned(),
        oid,
        display: display.to_owned(),
    }
}

fn check(code: &str, params: &[&str]) -> NativeQueryCheck {
    NativeQueryCheck {
        sql: format!(
            "WITH _cte_logimo_vali_ AS (\n{code}\n\n)\nSELECT \"id\", \"title\"\nFROM _cte_logimo_vali_"
        ),
        param_types: params.iter().map(|p| (*p).to_owned()).collect(),
    }
}

fn catalog() -> SourceCatalog {
    let mut native_queries = HashMap::new();
    native_queries.insert(
        check("SELECT $1 AS id, $2 AS title, $1 AS x", &["int4", "text"]),
        Ok(vec![
            column("id", "integer", 23),
            column("title", "text", 25),
            column("x", "integer", 23),
        ]),
    );
    native_queries.insert(
        check("SELEC 1", &[]),
        Err(
            json!({"description": null, "exec_status": "FatalError", "hint": null,
                   "message": "syntax error at or near \"SELEC\"", "status_code": "42601"}),
        ),
    );
    native_queries.insert(
        check("SELECT 'x' AS id, 't' AS title", &[]),
        Ok(vec![column("id", "text", 25), column("title", "text", 25)]),
    );
    SourceCatalog {
        types: HashMap::from([
            ("int4".to_owned(), named("int4", "integer", 23)),
            ("text".to_owned(), named("text", "text", 25)),
        ]),
        native_queries,
        ..Default::default()
    }
}

#[test]
fn parses_code() {
    assert_eq!(
        parse_code("a {{x}}{{ y }} b {{z"),
        vec![
            CodePart::Sql("a ".into()),
            CodePart::Arg("x".into()),
            CodePart::Arg("y".into()),
            CodePart::Sql(" b {{z".into()),
        ]
    );
}

#[test]
fn requests_types_and_checks() {
    let (types, checks) = requests(&source());
    assert_eq!(types, vec!["int4", "text"]);
    // Arguments are numbered by first use; queries without a model or with undeclared
    // arguments are not checked.
    assert_eq!(
        checks,
        vec![
            check("SELECT $1 AS id, $2 AS title, $1 AS x", &["int4", "text"]),
            check("SELEC 1", &[]),
            check("SELECT 'x' AS id, 't' AS title", &[]),
        ]
    );
}

#[test]
fn resolves_with_v2_errors() {
    let mut tables = Tables::default();
    let mut issues = vec![];
    resolve(&source(), &catalog(), &mut tables, &mut issues);

    let model = &tables.logical_models[0];
    assert_eq!(model.bool_exp_name(), "summary_bool_exp_bool_exp");
    assert_eq!(model.select_column_name(), "summary_enum_name");
    assert_eq!(model.display_name(), "summary");
    assert_eq!(model.columns.keys().collect::<Vec<_>>(), ["id", "title"]);
    assert!(!model.columns["id"].nullable);
    let name = logical_model_name("summary");
    assert!(tables.logical_model("default", &name).is_some());
    assert!(tables.get("default", &name).is_none());

    assert_eq!(tables.native_queries.len(), 1);
    let ok = &tables.native_queries[0];
    assert_eq!(ok.root_field, "ok");
    assert_eq!(ok.arguments.keys().collect::<Vec<_>>(), ["a", "b"]);
    assert!(!ok.arguments["b"].nullable);

    let issues: Vec<_> = issues.iter().map(InconsistentObject::to_json).collect();
    assert_eq!(
        issues,
        vec![
            json!({
                "definition": {"arguments": {}, "code": "SELECT 1", "returns": "nope", "root_field_name": "no_model"},
                "name": "native_query no_model in source default",
                "reason": "Inconsistent object: The logical model nope could not be found",
                "type": "native_query"
            }),
            json!({
                "definition": {"arguments": {}, "code": "SELECT {{x}}, {{y}}", "returns": "summary", "root_field_name": "undeclared"},
                "name": "native_query undeclared in source default",
                "reason": "Inconsistent object: The following columns are used in the query, but are not declared as arguments: ['x','y']",
                "type": "native_query"
            }),
            json!({
                "definition": {"arguments": {}, "code": "SELEC 1", "returns": "summary", "root_field_name": "broken"},
                "message": {
                    "arguments": [],
                    "error": {"description": null, "exec_status": "FatalError", "hint": null,
                              "message": "syntax error at or near \"SELEC\"", "status_code": "42601"},
                    "prepared": false,
                    "statement": "PREPARE _logimo_vali_ AS WITH _cte_logimo_vali_ AS (\nSELEC 1\n\n)\nSELECT \"id\", \"title\"\nFROM _cte_logimo_vali_"
                },
                "name": "native_query broken in source default",
                "reason": "Inconsistent object: Failed to validate query",
                "type": "native_query"
            }),
            json!({
                "definition": {"arguments": {}, "code": "SELECT 'x' AS id, 't' AS title", "returns": "summary", "root_field_name": "mismatch"},
                "message": "Return column 'id' has a type mismatch. The expected type is 'integer', but the actual type is 'text'.",
                "name": "native_query mismatch in source default",
                "reason": "Inconsistent object: Failed to validate query",
                "type": "native_query"
            }),
        ]
    );
}

#[test]
fn prepare_statement_lists_canonical_argument_types() {
    let c = check("SELECT $1", &["int4", "text"]);
    assert_eq!(
        prepare_statement(&c, &catalog()),
        "PREPARE _logimo_vali_(integer, text) AS WITH _cte_logimo_vali_ AS (\nSELECT $1\n\n)\nSELECT \"id\", \"title\"\nFROM _cte_logimo_vali_"
    );
}

#[test]
fn drops_native_queries_with_conflicting_root_fields() {
    let mut src = source();
    let queries = src.extra["native_queries"].as_array_mut().unwrap();
    let mut twin = queries[0].clone();
    twin["description"] = json!("Twin");
    queries.push(twin);
    let mut tables = Tables::default();
    let mut issues = vec![];
    resolve(&src, &catalog(), &mut tables, &mut issues);
    issues.clear();
    drop_conflicting(&mut tables, &mut issues);
    assert_eq!(tables.native_queries.len(), 1);
    assert_eq!(
        tables.native_queries[0].description.as_deref(),
        Some("Fine")
    );
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].name, "native_query ok in source default");
    assert!(
        issues[0].reason.contains("field 'ok'"),
        "{}",
        issues[0].reason
    );
}
