//! Immutable application state, rebuilt on metadata changes and swapped atomically.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use arc_swap::ArcSwap;
use hge_auth::AuthConfig;
use hge_core::RoleName;
use hge_metadata::Metadata;
use hge_postgres::PgSource;
use hge_schema::{RoleSchema, SchemaOptions, Tables, build_role_schema};

use crate::config::ServerConfig;

/// Everything derived from one metadata version.
pub struct AppState {
    pub metadata: Arc<Metadata>,
    pub tables: Arc<Tables>,
    pub sources: HashMap<String, Arc<PgSource>>,
    pub inconsistencies: Vec<String>,
    pub options: SchemaOptions,
    /// Deliverable event triggers of this metadata version.
    pub event_triggers: Arc<hge_events::EventTriggers>,
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
        tables: Tables,
        sources: HashMap<String, Arc<PgSource>>,
        inconsistencies: Vec<String>,
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
            tables: Arc::new(tables),
            sources,
            inconsistencies,
            options,
            event_triggers: Arc::default(),
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
    pub auth: AuthConfig,
    pub state: ArcSwap<AppState>,
}

impl Engine {
    pub fn new(config: ServerConfig, state: AppState) -> Self {
        let auth = AuthConfig {
            admin_secrets: config.admin_secrets.clone(),
            unauthorized_role: config.unauthorized_role.clone(),
        };
        Engine {
            config,
            auth,
            state: ArcSwap::from_pointee(state),
        }
    }
}
