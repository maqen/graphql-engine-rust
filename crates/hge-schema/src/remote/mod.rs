//! Remote schemas: their part of each role's schema and the lowering of remote fields to
//! outgoing GraphQL requests (spec 08). Owned by the *Remote schemas* workstream; fetching
//! and execution live in `hge-remote-schema`.

pub mod customize;
pub mod introspection;
mod lower;
pub mod metadata;
pub mod print;
mod schema;

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

pub use introspection::{RemoteDocument, parse_introspection};
pub use lower::lower_root;
pub(crate) use schema::add_remote_schemas;
pub use schema::check_conflicts;

/// What a remote schema field is, for the lowering step.
#[derive(Debug, Clone)]
pub enum RemoteField {
    /// A root field of a remote schema placed directly in a Hasura root type.
    Root {
        remote: Arc<RemoteSchemaInfo>,
        original: String,
    },
    /// The namespace field (`root_fields_namespace`) holding a remote's root fields.
    Namespace(Arc<RemoteSchemaInfo>),
    /// A field of a remote type (including the root fields under a namespace).
    Field { original: String },
}

/// A remote schema, introspected and customized.
pub struct RemoteSchemaInfo {
    pub name: Arc<str>,
    /// The remote's types under their original names.
    pub original: RemoteDocument,
    /// The remote's types under their customized names.
    pub customized: RemoteDocument,
    pub namespace: Option<String>,
    /// Customized type name → original name, for renamed types only.
    pub to_original: HashMap<String, String>,
    /// Original type name → customized name, for renamed types only (to rewrite the
    /// `__typename` values the remote returns).
    pub to_custom: Arc<HashMap<String, String>>,
}

impl fmt::Debug for RemoteSchemaInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RemoteSchemaInfo({})", self.name)
    }
}

impl RemoteSchemaInfo {
    /// Build from an introspection result: parse, validate, customize (spec 08 §3.3-§4).
    /// Errors are the messages V2 reports as `remote-schema-error`.
    pub fn new(
        name: &str,
        customization: Option<&metadata::Customization>,
        introspection: &serde_json::Value,
    ) -> Result<Self, String> {
        let original = parse_introspection(introspection)?;
        let customizer = customize::build(&original, customization);
        customize::validate(&original, &customizer)?;
        introspection::validate(&original)?;
        let customized = customize::apply(&original, &customizer);
        let to_original = customizer
            .types
            .iter()
            .map(|(k, v)| (v.clone(), k.clone()))
            .collect();
        Ok(RemoteSchemaInfo {
            name: name.into(),
            original,
            customized,
            namespace: customizer.namespace.clone(),
            to_original,
            to_custom: Arc::new(customizer.types),
        })
    }

    pub fn original_type<'a>(&'a self, customized: &'a str) -> &'a str {
        self.to_original
            .get(customized)
            .map_or(customized, String::as_str)
    }
}

/// The remote schemas of a metadata version that made it into the schema, in metadata
/// order.
#[derive(Debug, Clone, Default)]
pub struct RemoteSchemas {
    pub schemas: Vec<Arc<RemoteSchemaInfo>>,
}

#[cfg(test)]
mod tests;
