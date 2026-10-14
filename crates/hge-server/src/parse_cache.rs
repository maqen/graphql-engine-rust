//! Parsed GraphQL documents, keyed by query text.
//!
//! Clients send the same few query texts over and over, and parsing is a large share of the
//! engine's own work on a simple query (tens of microseconds). The parsed document depends
//! only on the text, so repeats reuse it. Only documents of requests that passed the
//! allowlist and validation are kept, and only for queries up to [`MAX_QUERY`] bytes: a
//! parsed document can be up to ~150x the size of its text, so the budget is small.

use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};
use std::sync::{Arc, RwLock};

use async_graphql_parser::types::ExecutableDocument;

/// Cached documents before the cache starts over.
const MAX_ENTRIES: usize = 1024;
/// Total size of cached query texts before the cache starts over (the parsed documents
/// take up to ~150x this).
const MAX_BYTES: usize = 512 << 10;
/// Longest query text that is cached.
const MAX_QUERY: usize = 16 << 10;

type Entries = HashMap<u64, (Box<str>, Arc<ExecutableDocument>)>;

#[derive(Default)]
struct Inner {
    entries: Entries,
    bytes: usize,
}

#[derive(Default)]
pub(crate) struct ParseCache {
    hasher: RandomState,
    inner: RwLock<Inner>,
}

impl ParseCache {
    /// The document of an earlier [`insert`](Self::insert) of the same text.
    pub(crate) fn get(&self, query: &str) -> Option<Arc<ExecutableDocument>> {
        if query.len() > MAX_QUERY {
            return None;
        }
        let hash = self.hasher.hash_one(query);
        self.inner
            .read()
            .expect("lock")
            .entries
            .get(&hash)
            .filter(|(text, _)| **text == *query)
            .map(|(_, doc)| doc.clone())
    }

    /// Keep `doc` as the parse of `query`.
    pub(crate) fn insert(&self, query: &str, doc: Arc<ExecutableDocument>) {
        if query.len() > MAX_QUERY {
            return;
        }
        let hash = self.hasher.hash_one(query);
        let mut dropped: Entries = HashMap::new();
        {
            let mut inner = self.inner.write().expect("lock");
            if inner.bytes + query.len() > MAX_BYTES || inner.entries.len() >= MAX_ENTRIES {
                // Freed after the lock is released.
                dropped = std::mem::take(&mut inner.entries);
                inner.bytes = 0;
            }
            inner.bytes += query.len();
            if let Some((old, _)) = inner.entries.insert(hash, (query.into(), doc)) {
                inner.bytes -= old.len();
            }
        }
        drop(dropped);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(q: &str) -> Arc<ExecutableDocument> {
        Arc::new(hge_schema::parse_query(q).unwrap())
    }

    #[test]
    fn returns_inserted_documents_of_equal_texts() {
        let cache = ParseCache::default();
        assert!(cache.get("{ a }").is_none());
        let a = parse("{ a }");
        cache.insert("{ a }", a.clone());
        assert!(Arc::ptr_eq(&a, &cache.get("{ a }").unwrap()));
        assert!(cache.get("{ b }").is_none());

        let long = format!("{{ a }}{}", " ".repeat(MAX_QUERY));
        cache.insert(&long, a.clone());
        assert!(cache.get(&long).is_none());
    }

    #[test]
    fn starts_over_when_full() {
        let cache = ParseCache::default();
        let doc = parse("{ a }");
        for i in 0..=MAX_ENTRIES {
            cache.insert(&format!("{{ f{i} }}"), doc.clone());
        }
        let inner = cache.inner.read().unwrap();
        assert_eq!(inner.entries.len(), 1);
        assert_eq!(inner.bytes, format!("{{ f{MAX_ENTRIES} }}").len());
    }
}
