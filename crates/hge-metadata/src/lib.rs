//! Hasura V2 metadata: the typed model and its loaders.
//!
//! * [`from_json`] — the `export_metadata` / `hdb_catalog.hdb_metadata` JSON document.
//! * [`load_dir`] — a Hasura CLI `metadata/` directory.

pub mod actions;
mod actions_sdl;
pub mod collections;
pub mod dir;
pub mod export;
pub mod model;
pub mod store;

use std::path::{Path, PathBuf};

pub use collections::*;
use hge_core::{Code, Error};
pub use model::*;

/// Error loading metadata from disk.
#[derive(Debug, thiserror::Error)]
#[error("{}: {message}", path.display())]
pub struct LoadError {
    pub path: PathBuf,
    pub message: String,
}

impl LoadError {
    pub fn at(path: impl AsRef<Path>, message: impl Into<String>) -> Self {
        LoadError {
            path: path.as_ref().to_path_buf(),
            message: message.into(),
        }
    }
}

pub use dir::{load_dir, load_dir_json};
pub use export::export_json;

/// Parse the canonical metadata JSON.
pub fn from_json(value: serde_json::Value) -> Result<Metadata, Error> {
    // Metadata documents without a `version` key are V1 (pre-sources) metadata.
    if let Some(obj) = value.as_object()
        && obj.contains_key("tables")
        && !obj.contains_key("sources")
    {
        return Err(Error::new(
            Code::NotSupported,
            "metadata version 1/2 (without sources) is not supported; \
             upgrade the project to config v3 with `hasura scripts update-project-v3`",
        ));
    }
    serde_path_to_error_lite(value)
}

/// Keys V2's `TableMetadata` parser accepts; anything else is an error there.
const TABLE_KEYS: &[&str] = &[
    "table",
    "is_enum",
    "configuration",
    "object_relationships",
    "array_relationships",
    "computed_fields",
    "remote_relationships",
    "insert_permissions",
    "select_permissions",
    "update_permissions",
    "delete_permissions",
    "event_triggers",
    "apollo_federation_config",
    "logical_model",
];

/// V2 rejects unknown keys in table metadata (the one place where it is strict).
fn check_table_keys(value: &serde_json::Value) -> Result<(), Error> {
    let sources = value.get("sources").and_then(|s| s.as_array());
    for (i, source) in sources.into_iter().flatten().enumerate() {
        let tables = source.get("tables").and_then(|t| t.as_array());
        for (j, table) in tables.into_iter().flatten().enumerate() {
            let Some(obj) = table.as_object() else {
                continue;
            };
            let unknown: Vec<String> = obj
                .keys()
                .filter(|k| !TABLE_KEYS.contains(&k.as_str()))
                .map(|k| format!("\"{k}\""))
                .collect();
            if !unknown.is_empty() {
                return Err(Error::new(
                    Code::ParseFailed,
                    format!(
                        "unexpected keys when parsing TableMetadata: [{}]",
                        unknown.join(",")
                    ),
                )
                .with_path(format!("$.sources[{i}].tables[{j}]")));
            }
        }
    }
    Ok(())
}

fn serde_path_to_error_lite(value: serde_json::Value) -> Result<Metadata, Error> {
    check_table_keys(&value)?;
    let mut md = serde_json::from_value::<Metadata>(value)
        .map_err(|e| Error::new(Code::ParseFailed, format!("invalid metadata: {e}")))?;
    actions::check_transforms(&md.actions)?;
    for source in &mut md.sources {
        if matches!(source.kind.as_str(), "postgres" | "pg") {
            source.configuration.fill_pg_defaults();
        }
    }
    Ok(md)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn remote_fk_needs_exactly_one_of_column_and_columns() {
        let fk = |v: serde_json::Value| serde_json::from_value::<RemoteFk>(v);
        assert!(fk(json!({"table": "a", "column": "x"})).is_ok());
        assert!(fk(json!({"table": "a", "columns": ["x", "y"]})).is_ok());
        for bad in [
            json!({"table": "a"}),
            json!({"table": "a", "column": "x", "columns": ["x"]}),
        ] {
            let e = fk(bad).unwrap_err().to_string();
            assert!(e.starts_with(REMOTE_FK_COLUMNS_ERROR), "{e}");
        }
    }

    #[test]
    fn parses_minimal_metadata_and_round_trips() {
        let doc = json!({
            "version": 3,
            "sources": [{
                "name": "default",
                "kind": "postgres",
                "tables": [
                    {
                        "table": {"schema": "public", "name": "author"},
                        "configuration": {"custom_name": "Author", "column_config": {"id": {"custom_name": "authorId"}}},
                        "array_relationships": [{
                            "name": "articles",
                            "using": {"foreign_key_constraint_on": {"column": "author_id", "table": {"schema": "public", "name": "article"}}}
                        }],
                        "select_permissions": [{
                            "role": "user",
                            "permission": {"columns": "*", "filter": {"id": {"_eq": "X-Hasura-User-Id"}}}
                        }]
                    },
                    {
                        "table": {"schema": "public", "name": "article"},
                        "object_relationships": [{"name": "author", "using": {"foreign_key_constraint_on": "author_id"}}]
                    }
                ],
                "configuration": {
                    "connection_info": {
                        "database_url": {"from_env": "HASURA_GRAPHQL_DATABASE_URL"},
                        "pool_settings": {"max_connections": 50, "idle_timeout": 180, "connection_lifetime": 600},
                        "use_prepared_statements": false,
                        "isolation_level": "read-committed"
                    }
                }
            }],
            "some_future_key": {"x": 1}
        });
        let md = from_json(doc.clone()).unwrap();
        let src = md.source("default").unwrap();
        assert_eq!(src.tables.len(), 2);
        assert_eq!(src.tables[0].configuration.column_name("id"), "authorId");
        assert!(matches!(
            src.tables[1].object_relationships[0].using,
            ObjectRelUsing::ForeignKey {
                foreign_key_constraint_on: ObjectFkOn::Column(_)
            }
        ));
        assert!(
            src.tables[0]
                .select_permission("user")
                .unwrap()
                .columns
                .contains("anything")
        );
        assert_eq!(serde_json::to_value(&md).unwrap(), doc);
    }
}
