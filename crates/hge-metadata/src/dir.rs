//! Loader for the Hasura CLI metadata directory (config v3 layout).
//!
//! ```text
//! metadata/
//!   version.yaml
//!   databases/databases.yaml          # sources; tables/functions via !include
//!   databases/<db>/tables/tables.yaml # list of !include "<schema>_<table>.yaml"
//!   actions.yaml + actions.graphql
//!   allow_list.yaml, api_limits.yaml, backend_configs.yaml, cron_triggers.yaml,
//!   graphql_schema_introspection.yaml, inherited_roles.yaml, metrics_config.yaml,
//!   network.yaml, opentelemetry.yaml, query_collections.yaml, remote_schemas.yaml,
//!   rest_endpoints.yaml
//! ```
//!
//! `!include` appears both as a YAML tag (`- !include "public_t1.yaml"`) and as a plain string
//! (`tables: "!include default/tables/tables.yaml"`); paths are relative to the including file.
//! The directory is assembled into the same JSON document `export_metadata` returns.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value as Json};
use serde_yaml::Value as Yaml;

use crate::actions_sdl;
use crate::{LoadError, Metadata};

/// Top-level files and the metadata key each one populates.
const SIMPLE_FILES: &[(&str, &str)] = &[
    ("remote_schemas.yaml", "remote_schemas"),
    ("query_collections.yaml", "query_collections"),
    ("allow_list.yaml", "allowlist"),
    ("cron_triggers.yaml", "cron_triggers"),
    ("inherited_roles.yaml", "inherited_roles"),
    ("rest_endpoints.yaml", "rest_endpoints"),
    ("api_limits.yaml", "api_limits"),
    ("metrics_config.yaml", "metrics_config"),
    ("network.yaml", "network"),
    ("backend_configs.yaml", "backend_configs"),
    ("opentelemetry.yaml", "opentelemetry"),
    (
        "graphql_schema_introspection.yaml",
        "graphql_schema_introspection",
    ),
];

/// Load a metadata directory into the canonical JSON document.
pub fn load_dir_json(dir: &Path) -> Result<Json, LoadError> {
    if !dir.is_dir() {
        return Err(LoadError::at(dir, "metadata directory does not exist"));
    }
    let mut root = Map::new();

    let version = match read_optional(&dir.join("version.yaml"))? {
        Some(Json::Object(obj)) => obj.get("version").cloned().unwrap_or(Json::from(3)),
        _ => Json::from(3),
    };
    root.insert("version".into(), version);

    let databases = dir.join("databases").join("databases.yaml");
    let sources = read_optional(&databases)?.unwrap_or(Json::Array(vec![]));
    root.insert("sources".into(), null_to_empty_array(sources));

    for (file, key) in SIMPLE_FILES {
        if let Some(value) = read_optional(&dir.join(file))?
            && !is_empty(&value)
        {
            root.insert((*key).into(), value);
        }
    }

    if let Some(actions_file) = read_optional(&dir.join("actions.yaml"))? {
        let mut actions_file = match actions_file {
            Json::Object(obj) => obj,
            Json::Null => Map::new(),
            _ => {
                return Err(LoadError::at(
                    dir.join("actions.yaml"),
                    "expected an object",
                ));
            }
        };
        let mut actions = match actions_file.remove("actions") {
            Some(Json::Array(a)) => a,
            _ => vec![],
        };
        let mut custom_types = actions_file
            .remove("custom_types")
            .filter(|v| !v.is_null())
            .unwrap_or_else(|| Json::Object(Map::new()));
        let sdl_path = dir.join("actions.graphql");
        if sdl_path.is_file() {
            let sdl = std::fs::read_to_string(&sdl_path)
                .map_err(|e| LoadError::at(&sdl_path, e.to_string()))?;
            actions_sdl::merge(&mut actions, &mut custom_types, &sdl)
                .map_err(|e| LoadError::at(&sdl_path, e))?;
        }
        if !actions.is_empty() {
            root.insert("actions".into(), Json::Array(actions));
        }
        if !is_empty_custom_types(&custom_types) {
            root.insert("custom_types".into(), custom_types);
        }
    }

    Ok(Json::Object(root))
}

/// Load and parse a metadata directory.
pub fn load_dir(dir: &Path) -> Result<Metadata, LoadError> {
    let json = load_dir_json(dir)?;
    crate::from_json(json).map_err(|e| LoadError::at(dir, e.message))
}

fn read_optional(path: &Path) -> Result<Option<Json>, LoadError> {
    if !path.is_file() {
        return Ok(None);
    }
    read_yaml(path).map(Some)
}

fn read_yaml(path: &Path) -> Result<Json, LoadError> {
    read_yaml_depth(path, 0)
}

fn read_yaml_depth(path: &Path, depth: usize) -> Result<Json, LoadError> {
    if depth > 32 {
        return Err(LoadError::at(path, "!include nesting too deep (cycle?)"));
    }
    let text = std::fs::read_to_string(path).map_err(|e| LoadError::at(path, e.to_string()))?;
    let yaml: Yaml = if text.trim().is_empty() {
        Yaml::Null
    } else {
        serde_yaml::from_str(&text).map_err(|e| LoadError::at(path, e.to_string()))?
    };
    let base = path.parent().map(Path::to_path_buf).unwrap_or_default();
    yaml_to_json(yaml, &base, path, depth)
}

/// Convert YAML to JSON, resolving `!include` relative to `base`.
fn yaml_to_json(yaml: Yaml, base: &Path, file: &Path, depth: usize) -> Result<Json, LoadError> {
    Ok(match yaml {
        Yaml::Null => Json::Null,
        Yaml::Bool(b) => Json::Bool(b),
        Yaml::Number(n) => {
            if let Some(i) = n.as_i64() {
                Json::from(i)
            } else if let Some(u) = n.as_u64() {
                Json::from(u)
            } else {
                n.as_f64()
                    .and_then(serde_json::Number::from_f64)
                    .map(Json::Number)
                    .unwrap_or(Json::Null)
            }
        }
        Yaml::String(s) => match s.strip_prefix("!include ") {
            Some(rel) => include(rel, base, depth)?,
            None => Json::String(s),
        },
        Yaml::Sequence(items) => Json::Array(
            items
                .into_iter()
                .map(|v| yaml_to_json(v, base, file, depth))
                .collect::<Result<_, _>>()?,
        ),
        Yaml::Mapping(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                let key = match k {
                    Yaml::String(s) => s,
                    Yaml::Bool(b) => b.to_string(),
                    Yaml::Number(n) => n.to_string(),
                    Yaml::Null => "null".to_owned(),
                    other => {
                        return Err(LoadError::at(
                            file,
                            format!("unsupported mapping key {other:?}"),
                        ));
                    }
                };
                out.insert(key, yaml_to_json(v, base, file, depth)?);
            }
            Json::Object(out)
        }
        Yaml::Tagged(tagged) => {
            if tagged.tag == "!include" || tagged.tag == "include" {
                match tagged.value {
                    Yaml::String(rel) => include(&rel, base, depth)?,
                    other => {
                        return Err(LoadError::at(
                            file,
                            format!("!include expects a file path, found {other:?}"),
                        ));
                    }
                }
            } else {
                yaml_to_json(tagged.value, base, file, depth)?
            }
        }
    })
}

fn include(rel: &str, base: &Path, depth: usize) -> Result<Json, LoadError> {
    let rel = rel.trim().trim_matches('"').trim_matches('\'');
    let path: PathBuf = base.join(rel);
    read_yaml_depth(&path, depth + 1)
}

fn null_to_empty_array(v: Json) -> Json {
    if v.is_null() { Json::Array(vec![]) } else { v }
}

fn is_empty(v: &Json) -> bool {
    match v {
        Json::Null => true,
        Json::Array(a) => a.is_empty(),
        Json::Object(o) => o.is_empty(),
        _ => false,
    }
}

fn is_empty_custom_types(v: &Json) -> bool {
    match v {
        Json::Object(o) => o.values().all(is_empty),
        other => is_empty(other),
    }
}
