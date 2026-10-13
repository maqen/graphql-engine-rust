//! Typed remote schema metadata (`remote_schemas[]`, spec 08 §1). Parsed from the
//! untyped entries `hge_metadata::Metadata::remote_schemas` keeps, so export round-trips
//! whatever was stored.

use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::Value as Json;

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteSchemaMetadata {
    pub name: String,
    pub definition: RemoteSchemaDef,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default)]
    pub permissions: Vec<RemoteSchemaPermission>,
    #[serde(default)]
    pub remote_relationships: Vec<Json>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteSchemaDef {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub url_from_env: Option<String>,
    #[serde(default)]
    pub headers: Vec<HeaderConf>,
    /// `None` (or `null`): introspection uses `headers`.
    #[serde(default)]
    pub introspection_headers: Option<Vec<HeaderConf>>,
    #[serde(default)]
    pub forward_client_headers: bool,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    #[serde(default)]
    pub customization: Option<Customization>,
}

impl RemoteSchemaDef {
    pub fn timeout_seconds(&self) -> u64 {
        self.timeout_seconds.unwrap_or(60)
    }

    /// `validateRemoteSchemaDef`.
    pub fn validate(&self) -> Result<(), String> {
        match (&self.url, &self.url_from_env) {
            (None, None) => return Err("both `url` and `url_from_env` can't be empty".into()),
            (Some(_), Some(_)) => {
                return Err("both `url` and `url_from_env` can't be present".into());
            }
            _ => {}
        }
        if let Some(c) = &self.customization {
            for f in c.field_names.iter().flatten() {
                for k in f.mapping.keys() {
                    if k.starts_with("__") {
                        return Err(format!("attempt to customize reserved field name \"{k}\""));
                    }
                }
            }
        }
        Ok(())
    }
}

/// A configured header: `{name, value}` or `{name, value_from_env}`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct HeaderConf {
    pub name: String,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub value_from_env: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Customization {
    #[serde(default)]
    pub root_fields_namespace: Option<String>,
    #[serde(default)]
    pub type_names: Option<TypeNamesCustomization>,
    #[serde(default)]
    pub field_names: Option<Vec<FieldNamesCustomization>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TypeNamesCustomization {
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub suffix: Option<String>,
    #[serde(default)]
    pub mapping: IndexMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FieldNamesCustomization {
    pub parent_type: String,
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub suffix: Option<String>,
    #[serde(default)]
    pub mapping: IndexMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteSchemaPermission {
    pub role: String,
    pub definition: RemoteSchemaPermissionDef,
    #[serde(default)]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteSchemaPermissionDef {
    pub schema: String,
}

/// Parse one `remote_schemas[]` entry.
pub fn parse(entry: &Json) -> Result<RemoteSchemaMetadata, String> {
    RemoteSchemaMetadata::deserialize(entry).map_err(|e| e.to_string())
}

/// The entry's name, even when the rest does not parse.
pub fn entry_name(entry: &Json) -> String {
    entry
        .get("name")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_owned()
}
