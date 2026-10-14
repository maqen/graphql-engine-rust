//! The seam between the metadata API and the running server.
//!
//! The API is written against [`Runtime`] so it never depends on the server crate: the server
//! implements it (connecting sources, introspecting, resolving and swapping in a new schema)
//! and tests can implement it with fakes.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;

use hge_core::{InconsistentObject, SourceCatalog};
use hge_metadata::store::MetadataStore;
use hge_metadata::{Metadata, Source};
use hge_postgres::PgSource;
use tokio::sync::MutexGuard;

/// Which sources a rebuild must reconnect and re-introspect.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Reload {
    /// Reuse pools and catalogs whose configuration did not change.
    #[default]
    Changed,
    /// Reuse unchanged pools but re-introspect every source.
    Introspect,
    /// Reconnect and re-introspect every source.
    All,
    /// Reconnect and re-introspect these sources.
    Sources(HashSet<String>),
}

impl Reload {
    /// Must this source's pool be recreated?
    pub fn reconnects(&self, source: &str) -> bool {
        match self {
            Reload::Changed | Reload::Introspect => false,
            Reload::All => true,
            Reload::Sources(s) => s.contains(source),
        }
    }

    /// Must this source be introspected again?
    pub fn introspects(&self, source: &str) -> bool {
        matches!(self, Reload::Introspect) || self.reconnects(source)
    }

    pub fn merge(&mut self, other: Reload) {
        *self = match (std::mem::take(self), other) {
            (Reload::All, _) | (_, Reload::All) => Reload::All,
            // Coarse but safe: everything is re-introspected and the named pools reconnect.
            (Reload::Introspect, Reload::Sources(_)) | (Reload::Sources(_), Reload::Introspect) => {
                Reload::All
            }
            (Reload::Introspect, _) | (_, Reload::Introspect) => Reload::Introspect,
            (Reload::Sources(mut a), Reload::Sources(b)) => {
                a.extend(b);
                Reload::Sources(a)
            }
            (Reload::Sources(a), Reload::Changed) | (Reload::Changed, Reload::Sources(a)) => {
                Reload::Sources(a)
            }
            (Reload::Changed, Reload::Changed) => Reload::Changed,
        };
    }
}

/// Options for [`Runtime::prepare`].
pub struct PrepareOptions<'a, P> {
    /// A previously prepared state to reuse pools and catalogs from (else the installed one).
    pub base: Option<&'a P>,
    pub reload: Reload,
    /// Catalogs to use as-is instead of introspecting (e.g. read inside a `run_sql`
    /// transaction before it commits).
    pub catalogs: HashMap<String, SourceCatalog>,
}

impl<P> Default for PrepareOptions<'_, P> {
    fn default() -> Self {
        PrepareOptions {
            base: None,
            reload: Reload::Changed,
            catalogs: HashMap::new(),
        }
    }
}

/// Server-wide switches the API honours.
#[derive(Debug, Clone, Default)]
pub struct ApiSettings {
    /// `HASURA_GRAPHQL_ENABLE_READ_ONLY_MODE`
    pub read_only: bool,
}

/// What the API needs from the server.
pub trait Runtime: Send + Sync {
    /// Sources connected, catalogs introspected and metadata resolved, ready to install.
    type Prepared: Clone + Send + Sync;

    /// Serialises metadata operations within this instance.
    fn lock(&self) -> impl Future<Output = MutexGuard<'_, ()>> + Send;

    fn store(&self) -> &dyn MetadataStore;

    /// Random per-process id, written with schema-sync notifications.
    fn instance_id(&self) -> &str;

    fn settings(&self) -> &ApiSettings;

    /// The installed metadata, its resource version and its inconsistencies.
    fn installed(&self) -> Installed;

    /// Connect, introspect and resolve `metadata` without installing it.
    fn prepare<'a>(
        &'a self,
        metadata: &'a Metadata,
        options: PrepareOptions<'a, Self::Prepared>,
    ) -> impl Future<Output = Self::Prepared> + Send + 'a;

    fn inconsistencies<'p>(&self, prepared: &'p Self::Prepared) -> &'p [InconsistentObject];

    /// A connected source of a prepared (or else the installed) state.
    fn source(&self, prepared: Option<&Self::Prepared>, name: &str) -> Option<Arc<PgSource>>;

    /// The introspected catalog of a source of a prepared (or else the installed) state.
    fn catalog(&self, prepared: Option<&Self::Prepared>, name: &str) -> Option<Arc<SourceCatalog>>;

    /// The metadata database, where cron and one-off scheduled events live (none when
    /// metadata is kept in memory).
    fn metadata_db(&self) -> Option<&deadpool_postgres::Pool> {
        None
    }

    /// Atomically swap in a prepared state. In-flight requests finish on the old one.
    fn install(&self, prepared: Self::Prepared, metadata: Metadata, resource_version: i64);

    /// After `pg_drop_source`: remove the event trigger SQL and catalog the engine created in
    /// the source's database (V2's `postDropSourceHook`). Failures are only logged. Like V2,
    /// it runs after the consistency check but before the new metadata is persisted.
    fn post_drop_source(
        &self,
        source: &Source,
        pg: Arc<PgSource>,
    ) -> impl Future<Output = ()> + Send {
        let _ = (source, pg);
        async {}
    }
}

/// A snapshot of the installed state.
#[derive(Debug, Clone)]
pub struct Installed {
    pub metadata: Arc<Metadata>,
    pub resource_version: i64,
    pub inconsistencies: Vec<InconsistentObject>,
}
