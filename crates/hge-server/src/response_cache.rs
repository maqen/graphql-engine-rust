//! Cached responses of operations that read no data.
//!
//! An operation whose root fields are all computed in process (introspection, `__typename`)
//! has a response that depends only on the role's schema and the request text, so it is
//! cached per metadata version (the cache lives in [`AppState`](crate::state::AppState) and
//! goes away with it). Introspection is the expensive case: validating, resolving and
//! serialising a full introspection query takes milliseconds, a cache hit microseconds.

use std::collections::HashMap;
use std::hash::{BuildHasher, Hash, Hasher, RandomState};
use std::sync::Mutex;

use hge_ir::{Operation, OperationKind, RootField};
use serde_json::Value as Json;

/// Total size of cached bodies before the cache starts over.
const MAX_BYTES: usize = 32 << 20;
/// Cached entries before the cache starts over.
const MAX_ENTRIES: usize = 1024;

/// What a cached response is keyed on, borrowed from the request.
#[derive(PartialEq, Eq)]
pub(crate) struct Key<'a> {
    /// Identifies the role schema the response was built from.
    pub schema: &'a crate::state::SchemaKey,
    pub query: &'a str,
    pub operation_name: Option<&'a str>,
    /// `None` when the request has no variables (or `null`).
    pub variables: Option<&'a Json>,
}

impl Hash for Key<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.schema.hash(state);
        self.query.hash(state);
        self.operation_name.hash(state);
        // Variables are rare on these operations; hash their text form when present.
        if let Some(v) = self.variables {
            v.to_string().hash(state);
        }
    }
}

struct Entry {
    schema: crate::state::SchemaKey,
    query: Box<str>,
    operation_name: Option<Box<str>>,
    variables: Option<Json>,
    body: String,
}

impl Entry {
    fn matches(&self, key: &Key<'_>) -> bool {
        self.schema == *key.schema
            && *self.query == *key.query
            && self.operation_name.as_deref() == key.operation_name
            && self.variables.as_ref() == key.variables
    }
}

#[derive(Default)]
struct Inner {
    entries: HashMap<u64, Entry>,
    bytes: usize,
}

/// Response bodies of data-free operations, keyed by (schema, query, operation, variables).
#[derive(Default)]
pub(crate) struct ResponseCache {
    hasher: RandomState,
    inner: Mutex<Inner>,
}

impl ResponseCache {
    fn hash(&self, key: &Key<'_>) -> u64 {
        self.hasher.hash_one(key)
    }

    pub(crate) fn get(&self, key: &Key<'_>) -> Option<String> {
        let hash = self.hash(key);
        let inner = self.inner.lock().expect("lock");
        inner
            .entries
            .get(&hash)
            .filter(|e| e.matches(key))
            .map(|e| e.body.clone())
    }

    /// Cache `body` as the response of `op` if `op` reads no data.
    pub(crate) fn insert_if_static(&self, key: &Key<'_>, op: &Operation, body: &str) {
        if !is_static(op) || body.len() > MAX_BYTES {
            return;
        }
        let hash = self.hash(key);
        let entry = Entry {
            schema: key.schema.clone(),
            query: key.query.into(),
            operation_name: key.operation_name.map(Into::into),
            variables: key.variables.cloned(),
            body: body.to_owned(),
        };
        let mut inner = self.inner.lock().expect("lock");
        if inner.bytes + body.len() > MAX_BYTES || inner.entries.len() >= MAX_ENTRIES {
            inner.entries.clear();
            inner.bytes = 0;
        }
        inner.bytes += body.len();
        if let Some(old) = inner.entries.insert(hash, entry) {
            inner.bytes -= old.body.len();
        }
    }
}

/// A query whose every root field is computed in process.
fn is_static(op: &Operation) -> bool {
    op.kind == OperationKind::Query
        && op
            .root_fields
            .iter()
            .all(|(_, rf)| matches!(rf, RootField::Static(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hge_core::RoleName;

    fn op(fields: Vec<RootField>, kind: OperationKind) -> Operation {
        Operation {
            kind,
            root_fields: fields
                .into_iter()
                .enumerate()
                .map(|(i, f)| (format!("f{i}"), f))
                .collect(),
            multiple_top_level_fields: false,
        }
    }

    #[test]
    fn caches_static_queries_only() {
        let cache = ResponseCache::default();
        let schema = Some((RoleName::admin(), false));
        let key = Key {
            schema: &schema,
            query: "{ __typename }",
            operation_name: None,
            variables: None,
        };
        let mutation = op(
            vec![RootField::Static(Json::String("x".into()))],
            OperationKind::Mutation,
        );
        cache.insert_if_static(&key, &mutation, "m");
        assert_eq!(cache.get(&key), None);

        let query = op(
            vec![RootField::Static(Json::String("x".into()))],
            OperationKind::Query,
        );
        cache.insert_if_static(&key, &query, "q");
        assert_eq!(cache.get(&key).as_deref(), Some("q"));

        // Any part of the key differing is a miss.
        let other_role = Some((RoleName::new("user"), false));
        let vars = serde_json::json!({"a": 1});
        for other in [
            Key {
                schema: &other_role,
                ..key
            },
            Key {
                schema: &None,
                ..key
            },
            Key {
                query: "{__typename}",
                ..key
            },
            Key {
                operation_name: Some("Q"),
                ..key
            },
            Key {
                variables: Some(&vars),
                ..key
            },
        ] {
            assert_eq!(cache.get(&other), None);
        }
    }

    #[test]
    fn starts_over_when_full() {
        let cache = ResponseCache::default();
        let schema = None;
        let query = op(vec![], OperationKind::Query);
        let texts: Vec<String> = (0..=MAX_ENTRIES).map(|i| format!("q{i}")).collect();
        for t in &texts {
            let key = Key {
                schema: &schema,
                query: t,
                operation_name: None,
                variables: None,
            };
            cache.insert_if_static(&key, &query, "body");
        }
        let inner = cache.inner.lock().unwrap();
        assert_eq!(inner.entries.len(), 1);
        assert_eq!(inner.bytes, 4);
    }
}
