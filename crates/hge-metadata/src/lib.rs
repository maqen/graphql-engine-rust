//! Hasura V2 metadata: the typed model and its loaders.
//!
//! * [`from_json`] — the `export_metadata` / `hdb_catalog.hdb_metadata` JSON document.
//! * [`load_dir`] — a Hasura CLI `metadata/` directory.

mod actions_sdl;
pub mod dir;
pub mod model;

use std::path::{Path, PathBuf};

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

fn serde_path_to_error_lite(value: serde_json::Value) -> Result<Metadata, Error> {
    serde_json::from_value::<Metadata>(value)
        .map_err(|e| Error::new(Code::ParseFailed, format!("invalid metadata: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
                        "pool_settings": {"max_connections": 50, "idle_timeout": 180}
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
