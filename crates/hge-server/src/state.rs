//! Immutable application state, rebuilt on metadata changes and swapped atomically.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, RwLock};

use arc_swap::ArcSwap;
use hge_actions::{ActionLogStore, ActionRegistry};
use hge_auth::Authenticator;
use hge_core::{InconsistentObject, RoleName, SourceCatalog};
use hge_metadata::Metadata;
use hge_metadata::store::MetadataStore;
use hge_metadata_api::{ApiSettings, Installed, PrepareOptions, Runtime};
use hge_postgres::PgSource;
use hge_schema::{ResolvedActions, RoleSchema, SchemaOptions, Tables, build_role_schema};

use crate::config::ServerConfig;
use crate::load::{self, Prepared};

/// A known role and whether the request uses backend-only permissions; `None` for every
/// unknown role.
pub(crate) type SchemaKey = Option<(RoleName, bool)>;

/// Everything derived from one metadata version.
pub struct AppState {
    pub metadata: Arc<Metadata>,
    /// The metadata's `resource_version` in the store.
    pub resource_version: i64,
    pub tables: Arc<Tables>,
    pub actions: Arc<ResolvedActions>,
    pub action_registry: Arc<ActionRegistry>,
    pub sources: HashMap<String, Arc<PgSource>>,
    pub inconsistencies: Vec<InconsistentObject>,
    pub options: SchemaOptions,
    /// Deliverable event triggers of this metadata version.
    pub event_triggers: Arc<hge_events::EventTriggers>,
    /// Connected sources and catalogs, reused by the next metadata change.
    pub prepared: Prepared,
    /// Resolved cron triggers of this metadata version.
    pub scheduled_triggers: Arc<hge_events::scheduled::ScheduledTriggers>,
    /// Roles named anywhere in metadata; every other role sees the same empty schema.
    known_roles: HashSet<RoleName>,
    /// Roles with a `backend_only` mutation permission: the only ones whose schema depends
    /// on whether a request uses backend-only permissions.
    backend_only_roles: HashSet<RoleName>,
    /// `graphql_schema_introspection.disabled_for_roles`
    introspection_disabled: HashSet<RoleName>,
    /// Built on first use. Keys are known roles (bounded by metadata) or `None` for all
    /// unknown roles, so request headers cannot grow this map.
    /// Known roles are keyed with whether the request uses backend-only permissions.
    schemas: RwLock<HashMap<SchemaKey, Arc<RoleSchema>>>,
    /// Responses of introspection-only queries, per schema.
    pub(crate) responses: crate::response_cache::ResponseCache,
    /// Parsed query documents by query text.
    pub(crate) documents: crate::parse_cache::ParseCache,
}

fn known_roles(metadata: &Metadata) -> HashSet<RoleName> {
    let mut roles = HashSet::new();
    for source in &metadata.sources {
        for t in &source.tables {
            let names = t
                .select_permissions
                .iter()
                .map(|p| &p.role)
                .chain(t.insert_permissions.iter().map(|p| &p.role))
                .chain(t.update_permissions.iter().map(|p| &p.role))
                .chain(t.delete_permissions.iter().map(|p| &p.role));
            roles.extend(names.map(RoleName::new));
        }
        roles.extend(hge_schema::native::permission_roles(source).map(RoleName::new));
    }
    for action in &metadata.actions {
        if let Some(perms) = action.get("permissions").and_then(|p| p.as_array()) {
            roles.extend(
                perms
                    .iter()
                    .filter_map(|p| p.get("role").and_then(|r| r.as_str()))
                    .map(RoleName::new),
            );
        }
    }
    for r in &metadata.inherited_roles {
        roles.insert(RoleName::new(&r.role_name));
    }
    roles.insert(RoleName::admin());
    roles
}

fn backend_only_roles(tables: &Tables) -> HashSet<RoleName> {
    tables
        .ordered
        .iter()
        .flat_map(|t| t.permissions.iter())
        .filter(|(_, p)| {
            p.insert.as_ref().is_some_and(|i| i.backend_only)
                || p.update.as_ref().is_some_and(|u| u.backend_only)
                || p.delete.as_ref().is_some_and(|d| d.backend_only)
        })
        .map(|(role, _)| role.clone())
        .collect()
}

fn introspection_disabled(metadata: &Metadata) -> HashSet<RoleName> {
    metadata
        .graphql_schema_introspection
        .as_ref()
        .and_then(|v| v.get("disabled_for_roles"))
        .and_then(|v| v.as_array())
        .map(|roles| {
            roles
                .iter()
                .filter_map(|r| r.as_str())
                .map(RoleName::new)
                .collect()
        })
        .unwrap_or_default()
}

impl AppState {
    pub fn new(
        metadata: Metadata,
        prepared: Prepared,
        resource_version: i64,
        options: SchemaOptions,
    ) -> Self {
        let introspection_disabled = introspection_disabled(&metadata);
        let mut known_roles = known_roles(&metadata);
        // These need a schema of their own (without introspection), even with no permissions.
        known_roles.extend(introspection_disabled.iter().cloned());
        let backend_only_roles = backend_only_roles(&prepared.tables);
        // Reuse the admin schema built while preparing (same tables, actions and options).
        let mut schemas = HashMap::new();
        if let Some(admin) = prepared.admin_schema.clone()
            && admin.options == options
            && !introspection_disabled.contains(&RoleName::admin())
        {
            schemas.insert(Some((RoleName::admin(), false)), admin);
        }
        let state = AppState {
            known_roles,
            backend_only_roles,
            introspection_disabled,
            metadata: Arc::new(metadata),
            resource_version,
            tables: prepared.tables.clone(),
            actions: prepared.actions.clone(),
            action_registry: prepared.action_registry.clone(),
            sources: prepared
                .sources
                .iter()
                .map(|(name, s)| (name.clone(), s.pg.clone()))
                .collect(),
            inconsistencies: prepared.inconsistencies.clone(),
            event_triggers: prepared.event_triggers.clone(),
            scheduled_triggers: prepared.scheduled_triggers.clone(),
            prepared,
            options,
            schemas: RwLock::new(schemas),
            responses: Default::default(),
            documents: Default::default(),
        };
        // Build the admin schema eagerly so the first request is fast and errors surface early.
        state.role_schema(&RoleName::admin(), false);
        state
    }

    /// Which cached schema a role uses: requests with equal keys see the same schema.
    pub(crate) fn schema_key(&self, role: &RoleName, backend_only: bool) -> SchemaKey {
        self.known_roles.contains(role).then(|| {
            (
                role.clone(),
                backend_only && self.backend_only_roles.contains(role),
            )
        })
    }

    /// The schema for a role, built on first use and cached for this metadata version.
    /// `backend_only`: the request uses backend-only permissions ([`Session::backend_only`]).
    ///
    /// [`Session::backend_only`]: hge_core::Session::backend_only
    pub fn role_schema(&self, role: &RoleName, backend_only: bool) -> Arc<RoleSchema> {
        let key = self.schema_key(role, backend_only);
        if let Some(s) = self.schemas.read().expect("lock").get(&key) {
            return s.clone();
        }
        // Unknown roles have no permissions; build for a name no permission can use.
        let (build_as, backend_only) = key.clone().unwrap_or_else(|| (RoleName::new(""), false));
        let mut built = build_role_schema(
            &self.tables,
            &self.actions,
            &self.prepared.remote_schemas,
            &build_as,
            backend_only,
            &self.options,
        );
        built.schema.introspection_enabled = !self.introspection_disabled.contains(&build_as);
        let built = Arc::new(built);
        let mut schemas = self.schemas.write().expect("lock");
        schemas.entry(key).or_insert(built).clone()
    }
}

/// The running engine: configuration plus the current state.
pub struct Engine {
    pub config: ServerConfig,
    pub auth: Authenticator,
    pub state: ArcSwap<AppState>,
    /// Where metadata is persisted (`hdb_catalog`, or in memory without a database).
    pub store: Arc<dyn MetadataStore>,
    /// Random per-process id for schema-sync notifications.
    pub instance_id: String,
    api_settings: ApiSettings,
    /// Serialises metadata changes (API requests and schema sync) within this instance.
    metadata_lock: tokio::sync::Mutex<()>,
    /// WebSocket connections and subscription pollers.
    pub ws: crate::ws::WebSockets,
    /// `hdb_catalog.hdb_action_log` (async actions); `None` without a metadata database.
    pub action_log: Option<Arc<dyn ActionLogStore>>,
    /// The metadata database, when metadata is stored in Postgres.
    pub metadata_db: Option<deadpool_postgres::Pool>,
}

impl Engine {
    pub fn new(
        config: ServerConfig,
        auth: Authenticator,
        store: Arc<dyn MetadataStore>,
        state: AppState,
    ) -> Self {
        let api_settings = ApiSettings {
            read_only: config.read_only,
        };
        Engine {
            ws: crate::ws::WebSockets::new(&config.websocket),
            config,
            auth,
            state: ArcSwap::from_pointee(state),
            store,
            instance_id: uuid::Uuid::new_v4().to_string(),
            api_settings,
            metadata_lock: tokio::sync::Mutex::new(()),
            action_log: None,
            metadata_db: None,
        }
    }
}

impl Runtime for Engine {
    type Prepared = Prepared;

    fn lock(&self) -> impl Future<Output = tokio::sync::MutexGuard<'_, ()>> + Send {
        self.metadata_lock.lock()
    }

    fn store(&self) -> &dyn MetadataStore {
        self.store.as_ref()
    }

    fn metadata_db(&self) -> Option<&deadpool_postgres::Pool> {
        self.metadata_db.as_ref()
    }

    fn instance_id(&self) -> &str {
        &self.instance_id
    }

    fn settings(&self) -> &ApiSettings {
        &self.api_settings
    }

    fn installed(&self) -> Installed {
        let state = self.state.load();
        Installed {
            metadata: state.metadata.clone(),
            resource_version: state.resource_version,
            inconsistencies: state.inconsistencies.clone(),
        }
    }

    async fn prepare(
        &self,
        metadata: &Metadata,
        options: PrepareOptions<'_, Prepared>,
    ) -> Prepared {
        let installed;
        let base = match options.base {
            Some(b) => b,
            None => {
                installed = self.state.load_full();
                &installed.prepared
            }
        };
        load::prepare(
            metadata,
            &self.config,
            Some(base),
            &options.reload,
            &options.remote_schemas,
            options.catalogs,
        )
        .await
    }

    fn inconsistencies<'p>(&self, prepared: &'p Prepared) -> &'p [InconsistentObject] {
        &prepared.inconsistencies
    }

    fn schema_error<'p>(&self, prepared: &'p Prepared) -> Option<&'p str> {
        prepared.tables.schema_error.as_deref()
    }

    fn source(&self, prepared: Option<&Prepared>, name: &str) -> Option<Arc<PgSource>> {
        match prepared {
            Some(p) => p.sources.get(name).map(|s| s.pg.clone()),
            None => self.state.load().sources.get(name).cloned(),
        }
    }

    fn catalog(&self, prepared: Option<&Prepared>, name: &str) -> Option<Arc<SourceCatalog>> {
        match prepared {
            Some(p) => p.sources.get(name).map(|s| s.catalog.clone()),
            None => self
                .state
                .load()
                .prepared
                .sources
                .get(name)
                .map(|s| s.catalog.clone()),
        }
    }

    async fn after_commit(&self, prepared: &Prepared) {
        for sync in prepared.trigger_sync.iter() {
            sync.run_deferred().await;
        }
    }

    async fn post_drop_source(&self, source: &hge_metadata::Source, pg: Arc<PgSource>) {
        let triggers: Vec<String> = source
            .tables
            .iter()
            .flat_map(|t| &t.event_triggers)
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(str::to_owned))
            .collect();
        if let Err(e) = hge_events::catalog::drop_source_catalog(pg.pool(), &triggers).await {
            tracing::warn!(
                target: "metadata",
                source = %source.name,
                error = %e,
                "Error executing cleanup actions after removing source '{}'. Consider cleaning up tables in hdb_catalog schema manually.",
                source.name
            );
        }
    }

    fn validate_remote_schema<'a>(
        &'a self,
        entry: &'a serde_json::Value,
        introspect: bool,
    ) -> impl Future<Output = Result<(), hge_core::Error>> + Send + 'a {
        crate::remote::validate(entry, introspect, &self.config)
    }

    fn remote_introspection(&self, prepared: Option<&Prepared>, name: &str) -> Option<Arc<str>> {
        let remote = |p: &Prepared| p.remotes.get(name).map(|r| r.raw_introspection.clone());
        match prepared {
            Some(p) => remote(p),
            None => remote(&self.state.load().prepared),
        }
    }

    fn table_roles(
        &self,
        prepared: Option<&Prepared>,
        source: &str,
        table: &hge_core::QualifiedTable,
    ) -> Option<Vec<String>> {
        let roles = |tables: &Tables| {
            tables.get(source, table).map(|t| {
                t.permissions
                    .keys()
                    .filter(|r| **r != RoleName::admin())
                    .map(|r| r.as_str().to_owned())
                    .collect()
            })
        };
        match prepared {
            Some(p) => roles(&p.tables),
            None => roles(&self.state.load().tables),
        }
    }

    fn install(&self, prepared: Prepared, metadata: Metadata, resource_version: i64) {
        load::log_prepared(&prepared);
        let state = AppState::new(
            metadata,
            prepared,
            resource_version,
            load::schema_options(&self.config),
        );
        self.state.store(Arc::new(state));
    }
}
