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

/// Which remote schemas a rebuild must introspect again, besides those whose definition
/// changed ([`Reload`] is about sources only).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum RemoteReload {
    #[default]
    Changed,
    All,
    Only(HashSet<String>),
}

impl RemoteReload {
    pub fn includes(&self, name: &str) -> bool {
        match self {
            RemoteReload::Changed => false,
            RemoteReload::All => true,
            RemoteReload::Only(names) => names.contains(name),
        }
    }

    pub fn merge(&mut self, other: RemoteReload) {
        *self = match (std::mem::take(self), other) {
            (RemoteReload::All, _) | (_, RemoteReload::All) => RemoteReload::All,
            (RemoteReload::Only(mut a), RemoteReload::Only(b)) => {
                a.extend(b);
                RemoteReload::Only(a)
            }
            (RemoteReload::Only(a), RemoteReload::Changed)
            | (RemoteReload::Changed, RemoteReload::Only(a)) => RemoteReload::Only(a),
            (RemoteReload::Changed, RemoteReload::Changed) => RemoteReload::Changed,
        };
    }
}

/// Options for [`Runtime::prepare`].
pub struct PrepareOptions<'a, P> {
    /// A previously prepared state to reuse pools and catalogs from (else the installed one).
    pub base: Option<&'a P>,
    pub reload: Reload,
    pub remote_schemas: RemoteReload,
    /// Catalogs to use as-is instead of introspecting (e.g. read inside a `run_sql`
    /// transaction before it commits).
    pub catalogs: HashMap<String, SourceCatalog>,
}

impl<P> Default for PrepareOptions<'_, P> {
    fn default() -> Self {
        PrepareOptions {
            base: None,
            reload: Reload::Changed,
            remote_schemas: RemoteReload::Changed,
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

    /// Why the GraphQL schema of a prepared state cannot be built, as V2 reports it; the
    /// metadata command fails with it whatever its inconsistency handling.
    fn schema_error<'p>(&self, _prepared: &'p Self::Prepared) -> Option<&'p str> {
        None
    }

    /// A connected source of a prepared (or else the installed) state.
    fn source(&self, prepared: Option<&Self::Prepared>, name: &str) -> Option<Arc<PgSource>>;

    /// The introspected catalog of a source of a prepared (or else the installed) state.
    fn catalog(&self, prepared: Option<&Self::Prepared>, name: &str) -> Option<Arc<SourceCatalog>>;

    /// Validate a `remote_schemas[]` entry and (with `introspect`) introspect its server, as
    /// `add_remote_schema` does before it touches the metadata (V2's `fetchRemoteSchema`):
    /// failures are the command's error, not an inconsistency.
    fn validate_remote_schema<'a>(
        &'a self,
        entry: &'a serde_json::Value,
        introspect: bool,
    ) -> impl Future<Output = Result<(), hge_core::Error>> + Send + 'a;

    /// The response of a remote schema of a prepared (or else the installed) state to the
    /// introspection query, verbatim; `None` unless the remote schema is consistent.
    fn remote_introspection(
        &self,
        prepared: Option<&Self::Prepared>,
        name: &str,
    ) -> Option<Arc<str>>;

    /// The metadata database, where cron and one-off scheduled events live (none when
    /// metadata is kept in memory).
    fn metadata_db(&self) -> Option<&deadpool_postgres::Pool> {
        None
    }

    /// Roles with a permission on a resolved table (inherited roles included, `admin` not)
    /// of a prepared (or else the installed) state; `None` when the table is not resolved.
    fn table_roles(
        &self,
        prepared: Option<&Self::Prepared>,
        source: &str,
        table: &hge_core::QualifiedTable,
    ) -> Option<Vec<String>>;

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
