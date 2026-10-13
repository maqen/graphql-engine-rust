//! Immutable application state, rebuilt on metadata changes and swapped atomically.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, RwLock};

use arc_swap::ArcSwap;
use hge_auth::Authenticator;
use hge_core::{InconsistentObject, RoleName, SourceCatalog};
use hge_metadata::Metadata;
use hge_metadata::store::MetadataStore;
use hge_metadata_api::{ApiSettings, Installed, PrepareOptions, Runtime};
use hge_postgres::PgSource;
use hge_schema::{RoleSchema, SchemaOptions, Tables, build_role_schema};

use crate::config::ServerConfig;
use crate::load::{self, Prepared};

/// Everything derived from one metadata version.
pub struct AppState {
    pub metadata: Arc<Metadata>,
    /// The metadata's `resource_version` in the store.
    pub resource_version: i64,
    pub tables: Arc<Tables>,
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
    /// `graphql_schema_introspection.disabled_for_roles`
    introspection_disabled: HashSet<RoleName>,
    /// Built on first use. Keys are known roles (bounded by metadata) or `None` for all
    /// unknown roles, so request headers cannot grow this map.
    schemas: RwLock<HashMap<Option<RoleName>, Arc<RoleSchema>>>,
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
    }
    for r in &metadata.inherited_roles {
        roles.insert(RoleName::new(&r.role_name));
    }
    roles.insert(RoleName::admin());
    roles
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
        let state = AppState {
            known_roles,
            introspection_disabled,
            metadata: Arc::new(metadata),
            resource_version,
            tables: prepared.tables.clone(),
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
            schemas: RwLock::new(HashMap::new()),
        };
        // Build the admin schema eagerly so the first request is fast and errors surface early.
        state.role_schema(&RoleName::admin());
        state
    }

    /// The schema for a role, built on first use and cached for this metadata version.
    pub fn role_schema(&self, role: &RoleName) -> Arc<RoleSchema> {
        let key = self.known_roles.contains(role).then(|| role.clone());
        if let Some(s) = self.schemas.read().expect("lock").get(&key) {
            return s.clone();
        }
        // Unknown roles have no permissions; build for a name no permission can use.
        let build_as = key.clone().unwrap_or_else(|| RoleName::new(""));
        let mut built = build_role_schema(&self.tables, &build_as, &self.options);
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
            config,
            auth,
            state: ArcSwap::from_pointee(state),
            store,
            instance_id: uuid::Uuid::new_v4().to_string(),
            api_settings,
            metadata_lock: tokio::sync::Mutex::new(()),
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
            options.catalogs,
        )
        .await
    }

    fn inconsistencies<'p>(&self, prepared: &'p Prepared) -> &'p [InconsistentObject] {
        &prepared.inconsistencies
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
