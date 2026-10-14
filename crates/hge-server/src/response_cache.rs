//! Cached responses of operations that read no data.
//!
//! An operation whose root fields are all computed in process (introspection, `__typename`)
//! has a response that depends only on the role's schema and the request text, so it is
//! cached per metadata version (the cache lives in [`AppState`](crate::state::AppState) and
//! goes away with it). Introspection is the expensive case: validating, resolving and
//! serialising a full introspection query takes milliseconds, a cache hit microseconds.
//!
//! Only requests without variables and with a query of at most [`MAX_QUERY`] bytes are
//! cached, which keeps both the lookup on every request and the cached keys small.

use std::collections::HashMap;
use std::hash::{BuildHasher, Hash, RandomState};
use std::sync::{Arc, RwLock};

use hge_ir::{Operation, OperationKind, RootField};

/// Total size of cached queries and bodies before the cache starts over.
const MAX_BYTES: usize = 32 << 20;
/// Cached entries before the cache starts over.
const MAX_ENTRIES: usize = 1024;
/// Longest query text that is cached (a full introspection query is about 2 KB).
pub(crate) const MAX_QUERY: usize = 64 << 10;

/// What a cached response is keyed on, borrowed from the request.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Key<'a> {
    /// Identifies the role schema the response was built from.
    pub schema: &'a crate::state::SchemaKey,
    pub query: &'a str,
    pub operation_name: Option<&'a str>,
}

struct Entry {
    schema: crate::state::SchemaKey,
    query: Box<str>,
    operation_name: Option<Box<str>>,
    body: Arc<str>,
}

impl Entry {
    fn matches(&self, key: &Key<'_>) -> bool {
        self.schema == *key.schema
            && *self.query == *key.query
            && self.operation_name.as_deref() == key.operation_name
    }

    fn size(&self) -> usize {
        self.query.len() + self.body.len()
    }
}

#[derive(Default)]
struct Inner {
    entries: HashMap<u64, Entry>,
    bytes: usize,
}

/// Response bodies of data-free operations, keyed by (schema, query, operation name).
#[derive(Default)]
pub(crate) struct ResponseCache {
    hasher: RandomState,
    inner: RwLock<Inner>,
}

impl ResponseCache {
    fn hash(&self, key: &Key<'_>) -> u64 {
        self.hasher.hash_one(key)
    }

    pub(crate) fn get(&self, key: &Key<'_>) -> Option<Arc<str>> {
        let inner = self.inner.read().expect("lock");
        if inner.entries.is_empty() {
            return None;
        }
        inner
            .entries
            .get(&self.hash(key))
            .filter(|e| e.matches(key))
            .map(|e| e.body.clone())
    }

    /// Cache `body` as the response of `op` if `op` reads no data.
    pub(crate) fn insert_if_static(&self, key: &Key<'_>, op: &Operation, body: &str) {
        if !is_static(op) || key.query.len() > MAX_QUERY || body.len() > MAX_BYTES {
            return;
        }
        let hash = self.hash(key);
        let entry = Entry {
            schema: key.schema.clone(),
            query: key.query.into(),
            operation_name: key.operation_name.map(Into::into),
            body: body.into(),
        };
        let size = entry.size();
        let mut inner = self.inner.write().expect("lock");
        if inner.bytes + size > MAX_BYTES || inner.entries.len() >= MAX_ENTRIES {
            inner.entries.clear();
            inner.bytes = 0;
        }
        inner.bytes += size;
        if let Some(old) = inner.entries.insert(hash, entry) {
            inner.bytes -= old.size();
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
    use serde_json::Value as Json;

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
        let schema = (Some((RoleName::admin(), false)), false);
        let key = Key {
            schema: &schema,
            query: "{ __typename }",
            operation_name: None,
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

        // Long queries are not cached.
        let long = "#".repeat(MAX_QUERY + 1);
        let long_key = Key {
            query: &long,
            ..key
        };
        cache.insert_if_static(&long_key, &query, "q");
        assert_eq!(cache.get(&long_key), None);

        // Any part of the key differing is a miss.
        let other_role = (Some((RoleName::new("user"), false)), false);
        let unknown_role = (None, false);
        let relay = (Some((RoleName::admin(), false)), true);
        for other in [
            Key {
                schema: &other_role,
                ..key
            },
            Key {
                schema: &unknown_role,
                ..key
            },
            Key {
                schema: &relay,
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
        ] {
            assert_eq!(cache.get(&other), None);
        }
    }

    #[test]
    fn starts_over_when_full() {
        let cache = ResponseCache::default();
        let schema = (None, false);
        let query = op(vec![], OperationKind::Query);
        let texts: Vec<String> = (0..=MAX_ENTRIES).map(|i| format!("q{i}")).collect();
        for t in &texts {
            let key = Key {
                schema: &schema,
                query: t,
                operation_name: None,
            };
            cache.insert_if_static(&key, &query, "body");
        }
        let inner = cache.inner.read().unwrap();
        assert_eq!(inner.entries.len(), 1);
        assert_eq!(inner.bytes, texts[MAX_ENTRIES].len() + 4);
    }
}
