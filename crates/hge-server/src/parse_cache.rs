//! Parsed GraphQL documents, keyed by query text.
//!
//! Clients send the same few query texts over and over, and parsing is a large share of the
//! engine's own work on a simple query (tens of microseconds). The parsed document depends
//! only on the text, so repeats reuse it. Only successful parses of queries up to
//! [`MAX_QUERY`] bytes are kept; errors always go through the parser again.

use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};
use std::sync::{Arc, RwLock};

use async_graphql_parser::types::ExecutableDocument;
use hge_core::Error;

/// Cached documents before the cache starts over.
const MAX_ENTRIES: usize = 1024;
/// Total size of cached query texts before the cache starts over.
const MAX_BYTES: usize = 16 << 20;
/// Longest query text that is cached.
const MAX_QUERY: usize = 64 << 10;

#[derive(Default)]
struct Inner {
    entries: HashMap<u64, (Box<str>, Arc<ExecutableDocument>)>,
    bytes: usize,
}

#[derive(Default)]
pub(crate) struct ParseCache {
    hasher: RandomState,
    inner: RwLock<Inner>,
}

impl ParseCache {
    /// Parse `query`, or return the document of an earlier parse of the same text.
    pub(crate) fn parse(&self, query: &str) -> Result<Arc<ExecutableDocument>, Error> {
        if query.len() > MAX_QUERY {
            return hge_schema::parse_query(query).map(Arc::new);
        }
        let hash = self.hasher.hash_one(query);
        if let Some((_, doc)) = self
            .inner
            .read()
            .expect("lock")
            .entries
            .get(&hash)
            .filter(|(text, _)| **text == *query)
        {
            return Ok(doc.clone());
        }
        let doc = Arc::new(hge_schema::parse_query(query)?);
        let mut inner = self.inner.write().expect("lock");
        if inner.bytes + query.len() > MAX_BYTES || inner.entries.len() >= MAX_ENTRIES {
            inner.entries.clear();
            inner.bytes = 0;
        }
        inner.bytes += query.len();
        if let Some((old, _)) = inner.entries.insert(hash, (query.into(), doc.clone())) {
            inner.bytes -= old.len();
        }
        Ok(doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reuses_documents_of_equal_texts() {
        let cache = ParseCache::default();
        let a = cache.parse("{ a }").unwrap();
        let b = cache.parse("{ a }").unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        let c = cache.parse("{ b }").unwrap();
        assert!(!Arc::ptr_eq(&a, &c));
        assert!(cache.parse("{").is_err());
        assert!(cache.parse("{").is_err());
        assert_eq!(cache.inner.read().unwrap().entries.len(), 2);
    }

    #[test]
    fn starts_over_when_full() {
        let cache = ParseCache::default();
        for i in 0..=MAX_ENTRIES {
            cache.parse(&format!("{{ f{i} }}")).unwrap();
        }
        let inner = cache.inner.read().unwrap();
        assert_eq!(inner.entries.len(), 1);
        assert_eq!(inner.bytes, format!("{{ f{MAX_ENTRIES} }}").len());
    }
}
