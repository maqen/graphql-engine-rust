//! Remote schema root fields: GraphQL requests to forward to a remote server.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Map, Value as Json};

/// A root field answered by a remote schema.
#[derive(Debug, Clone)]
pub struct RemoteRootField {
    /// The remote schema's name in metadata.
    pub remote_schema: Arc<str>,
    pub kind: RemoteRootKind,
}

#[derive(Debug, Clone)]
pub enum RemoteRootKind {
    /// One remote root field: one request.
    Field(RemoteCall),
    /// The namespace field of a remote (`root_fields_namespace`): one request per selected
    /// field, assembled into an object under the namespace's response key.
    Namespace(Vec<(String, NamespaceItem)>),
}

#[derive(Debug, Clone)]
pub enum NamespaceItem {
    Call(RemoteCall),
    /// `__typename`, answered locally.
    Static(Json),
}

/// One outgoing GraphQL request selecting a single root field.
#[derive(Debug, Clone)]
pub struct RemoteCall {
    /// The printed operation.
    pub query: String,
    /// The client's `operationName`, when it sent one.
    pub operation_name: Option<String>,
    /// Variables used by the operation (`None`: no `variables` key is sent).
    pub variables: Option<Map<String, Json>>,
    /// The key of the field in the remote's `data`.
    pub response_key: String,
    /// How to rewrite `__typename` values in the response (customized type names).
    pub typenames: Option<TypenameRewrite>,
}

/// Rewrites original type names to customized ones in a remote response.
#[derive(Debug, Clone)]
pub struct TypenameRewrite {
    /// Original type name → customized name.
    pub map: Arc<HashMap<String, String>>,
    /// Where `__typename` values are, below the root field's value.
    pub tree: TypenameTree,
}

/// The response keys of a selection set holding `__typename` values, and the nested
/// selections that contain some.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TypenameTree {
    pub typename_keys: Vec<String>,
    pub children: Vec<(String, TypenameTree)>,
}

impl TypenameTree {
    pub fn is_empty(&self) -> bool {
        self.typename_keys.is_empty() && self.children.is_empty()
    }

    /// Rewrite `value` (the value of the field this tree describes) in place.
    pub fn apply(&self, value: &mut Json, map: &HashMap<String, String>) {
        match value {
            Json::Array(items) => {
                for item in items {
                    self.apply(item, map);
                }
            }
            Json::Object(obj) => {
                for key in &self.typename_keys {
                    if let Some(Json::String(t)) = obj.get_mut(key)
                        && let Some(new) = map.get(t.as_str())
                    {
                        *t = new.clone();
                    }
                }
                for (key, child) in &self.children {
                    if let Some(v) = obj.get_mut(key) {
                        child.apply(v, map);
                    }
                }
            }
            _ => {}
        }
    }
}

/// A remote relationship field of a row (spec 08 §9). The database returns the row's join
/// fields in its place (an object of `lhs`); the executor then replaces that object with
/// what the target returns for those values.
#[derive(Debug, Clone)]
pub struct RemoteJoin {
    /// The join fields by their names in the relationship (`lhs_fields`), and how to read
    /// them from the row.
    pub lhs: Vec<(String, crate::Field)>,
    pub target: Arc<dyn RemoteJoinTarget>,
}

/// The other side of a [`RemoteJoin`].
pub trait RemoteJoinTarget: std::fmt::Debug + Send + Sync {
    /// The remote schema the join calls.
    fn remote_schema(&self) -> &str;

    /// One request for all of a join's distinct arguments (join field values by name),
    /// each under the alias `f<id>`.
    fn request(
        &self,
        arguments: &[(usize, &Map<String, Json>)],
    ) -> Result<RemoteCall, hge_core::Error>;

    /// The value for argument `id` in the remote's `data`.
    fn extract(&self, data: &Map<String, Json>, id: usize) -> Result<Json, hge_core::Error>;
}
