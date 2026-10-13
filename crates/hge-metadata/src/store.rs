//! Where metadata is persisted: the [`MetadataStore`] seam.
//!
//! The production implementation keeps metadata in `hdb_catalog.hdb_metadata` exactly like V2
//! (see `hge-metadata-api`'s `PgCatalog`), so a database used by a V2 instance boots with its
//! metadata unchanged. [`MemoryStore`] backs servers without a metadata database and tests.
//!
//! This module is IO-free: implementations bring their own runtime.

use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use hge_core::{Code, Error};
use serde_json::{Value, json};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Metadata as stored, with its optimistic-locking version.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredMetadata {
    pub metadata: Value,
    pub resource_version: i64,
}

/// `hdb_catalog.hdb_version`'s CLI / console state (`get_catalog_state`).
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogState {
    pub id: String,
    pub cli_state: Value,
    pub console_state: Value,
}

impl CatalogState {
    pub fn to_json(&self) -> Value {
        json!({ "id": self.id, "cli_state": self.cli_state, "console_state": self.console_state })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogStateKind {
    Cli,
    Console,
}

/// Which caches another instance must rebuild after a change (V2's `CacheInvalidations`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Invalidations {
    pub metadata: bool,
    pub remote_schemas: Vec<String>,
    pub sources: Vec<String>,
    pub data_connectors: Vec<String>,
}

impl Invalidations {
    pub fn to_json(&self) -> Value {
        json!({
            "metadata": self.metadata,
            "remote_schemas": self.remote_schemas,
            "sources": self.sources,
            "data_connectors": self.data_connectors,
        })
    }

    pub fn from_json(v: &Value) -> Self {
        let names = |k: &str| {
            v.get(k)
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        };
        Invalidations {
            metadata: v.get("metadata").and_then(Value::as_bool).unwrap_or(false),
            remote_schemas: names("remote_schemas"),
            sources: names("sources"),
            data_connectors: names("data_connectors"),
        }
    }
}

/// `409 conflict` for a stale `resource_version`.
pub fn conflict(given: i64) -> Error {
    Error::new(
        Code::Conflict,
        format!("metadata resource version referenced ({given}) did not match current version"),
    )
}

/// Persistent metadata storage with optimistic concurrency.
pub trait MetadataStore: Send + Sync {
    /// The stored metadata; `None` when nothing is stored yet (resource version 0).
    fn fetch(&self) -> BoxFuture<'_, Result<Option<StoredMetadata>, Error>>;

    /// Store `metadata` if the stored version still equals `expected` (or nothing is stored
    /// yet). Returns the new resource version; a stale `expected` fails with [`conflict`].
    fn store<'a>(&'a self, metadata: &'a Value, expected: i64)
    -> BoxFuture<'a, Result<i64, Error>>;

    /// Bump the version without changing the metadata (`reload_metadata`).
    fn bump_version(&self, expected: i64) -> BoxFuture<'_, Result<i64, Error>>;

    /// The stored resource version only (cheap; polled for multi-instance sync).
    fn resource_version(&self) -> BoxFuture<'_, Result<i64, Error>>;

    /// Tell other instances about a change (`hdb_catalog.hdb_schema_notifications`).
    fn notify<'a>(
        &'a self,
        resource_version: i64,
        invalidations: &'a Invalidations,
        instance_id: &'a str,
    ) -> BoxFuture<'a, Result<(), Error>>;

    /// The latest notification newer than `after` written by another instance.
    fn notification_since<'a>(
        &'a self,
        after: i64,
        instance_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<(i64, Invalidations)>, Error>>;

    fn catalog_state(&self) -> BoxFuture<'_, Result<CatalogState, Error>>;

    fn set_catalog_state<'a>(
        &'a self,
        kind: CatalogStateKind,
        state: &'a Value,
    ) -> BoxFuture<'a, Result<(), Error>>;
}

/// In-process store, for servers without a metadata database and for tests.
#[derive(Debug)]
pub struct MemoryStore {
    inner: Mutex<MemoryInner>,
}

#[derive(Debug)]
struct MemoryInner {
    metadata: Option<StoredMetadata>,
    catalog: CatalogState,
    notification: Option<(i64, Invalidations, String)>,
}

impl Default for MemoryStore {
    fn default() -> Self {
        MemoryStore {
            inner: Mutex::new(MemoryInner {
                metadata: None,
                catalog: CatalogState {
                    id: "00000000-0000-0000-0000-000000000000".to_owned(),
                    cli_state: json!({}),
                    console_state: json!({}),
                },
                notification: None,
            }),
        }
    }
}

impl MemoryStore {
    fn with<T>(&self, f: impl FnOnce(&mut MemoryInner) -> T) -> T {
        f(&mut self.inner.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

impl MetadataStore for MemoryStore {
    fn fetch(&self) -> BoxFuture<'_, Result<Option<StoredMetadata>, Error>> {
        let r = self.with(|i| i.metadata.clone());
        Box::pin(async move { Ok(r) })
    }

    fn store<'a>(
        &'a self,
        metadata: &'a Value,
        expected: i64,
    ) -> BoxFuture<'a, Result<i64, Error>> {
        let r = self.with(|i| match &mut i.metadata {
            None => {
                i.metadata = Some(StoredMetadata {
                    metadata: metadata.clone(),
                    resource_version: 1,
                });
                Ok(1)
            }
            Some(s) if s.resource_version == expected => {
                s.metadata = metadata.clone();
                s.resource_version += 1;
                Ok(s.resource_version)
            }
            Some(_) => Err(conflict(expected)),
        });
        Box::pin(async move { r })
    }

    fn bump_version(&self, expected: i64) -> BoxFuture<'_, Result<i64, Error>> {
        let r = self.with(|i| match &mut i.metadata {
            Some(s) if s.resource_version == expected => {
                s.resource_version += 1;
                Ok(s.resource_version)
            }
            _ => Err(conflict(expected)),
        });
        Box::pin(async move { r })
    }

    fn resource_version(&self) -> BoxFuture<'_, Result<i64, Error>> {
        let r = self.with(|i| i.metadata.as_ref().map_or(0, |s| s.resource_version));
        Box::pin(async move { Ok(r) })
    }

    fn notify<'a>(
        &'a self,
        resource_version: i64,
        invalidations: &'a Invalidations,
        instance_id: &'a str,
    ) -> BoxFuture<'a, Result<(), Error>> {
        self.with(|i| {
            i.notification = Some((
                resource_version,
                invalidations.clone(),
                instance_id.to_owned(),
            ))
        });
        Box::pin(async { Ok(()) })
    }

    fn notification_since<'a>(
        &'a self,
        after: i64,
        instance_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<(i64, Invalidations)>, Error>> {
        let r = self.with(|i| match &i.notification {
            Some((v, inv, id)) if *v > after && id != instance_id => Some((*v, inv.clone())),
            _ => None,
        });
        Box::pin(async move { Ok(r) })
    }

    fn catalog_state(&self) -> BoxFuture<'_, Result<CatalogState, Error>> {
        let r = self.with(|i| i.catalog.clone());
        Box::pin(async move { Ok(r) })
    }

    fn set_catalog_state<'a>(
        &'a self,
        kind: CatalogStateKind,
        state: &'a Value,
    ) -> BoxFuture<'a, Result<(), Error>> {
        self.with(|i| match kind {
            CatalogStateKind::Cli => i.catalog.cli_state = state.clone(),
            CatalogStateKind::Console => i.catalog.console_state = state.clone(),
        });
        Box::pin(async { Ok(()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_on<T>(f: BoxFuture<'_, T>) -> T {
        // The memory store's futures are always ready.
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        let mut f = f;
        match f.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(v) => v,
            std::task::Poll::Pending => unreachable!(),
        }
    }

    #[test]
    fn optimistic_locking() {
        let store = MemoryStore::default();
        let md = json!({"version": 3, "sources": []});
        assert_eq!(block_on(store.resource_version()).unwrap(), 0);
        assert_eq!(block_on(store.store(&md, 0)).unwrap(), 1);
        assert_eq!(block_on(store.store(&md, 1)).unwrap(), 2);
        let err = block_on(store.store(&md, 1)).unwrap_err();
        assert_eq!(err.code, Code::Conflict);
        assert_eq!(block_on(store.bump_version(2)).unwrap(), 3);
    }

    #[test]
    fn notifications_skip_own_instance() {
        let store = MemoryStore::default();
        let inv = Invalidations {
            metadata: true,
            ..Default::default()
        };
        block_on(store.notify(5, &inv, "a")).unwrap();
        assert_eq!(block_on(store.notification_since(4, "a")).unwrap(), None);
        assert_eq!(
            block_on(store.notification_since(4, "b")).unwrap(),
            Some((5, inv.clone()))
        );
        assert_eq!(block_on(store.notification_since(5, "b")).unwrap(), None);
    }
}
