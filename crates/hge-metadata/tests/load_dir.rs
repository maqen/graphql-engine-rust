use std::path::Path;

use hge_metadata::{ArrayRelUsing, Columns, load_dir};
use serde_json::json;

#[test]
fn loads_cli_project_directory() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/project/metadata");
    let md = load_dir(&dir).expect("metadata loads");
    assert_eq!(md.version, 3);
    let src = md.source("default").expect("default source");
    assert_eq!(src.tables.len(), 2);
    assert_eq!(src.tables[0].table.name, "author");
    assert!(matches!(
        &src.tables[0].array_relationships[0].using,
        ArrayRelUsing::ForeignKey { foreign_key_constraint_on } if foreign_key_constraint_on.table.name == "article"
    ));
    let perm = src.tables[1].select_permission("user").unwrap();
    assert_eq!(
        perm.columns,
        Columns::List(vec!["id".into(), "title".into()])
    );
    assert_eq!(perm.filter, json!({"is_published": {"_eq": true}}));

    assert_eq!(md.actions.len(), 1);
    assert_eq!(md.actions[0]["definition"]["output_type"], "LoginOutput");
    assert_eq!(md.actions[0]["permissions"], json!([{"role": "anonymous"}]));
    assert_eq!(
        md.custom_types.as_ref().unwrap()["objects"][0]["fields"],
        json!([{"name": "accessToken", "type": "String!"}])
    );
    // empty files do not produce keys
    let exported = serde_json::to_value(&md).unwrap();
    assert!(exported.get("allowlist").is_none());
    assert!(exported.get("api_limits").is_none());
}
