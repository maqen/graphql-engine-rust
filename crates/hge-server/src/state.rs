//! Immutable application state, rebuilt on metadata changes and swapped atomically.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use arc_swap::ArcSwap;
use hge_auth::AuthConfig;
use hge_core::RoleName;
use hge_metadata::Metadata;
use hge_postgres::PgSource;
use hge_schema::{RoleSchema, SchemaOptions, Tables, build_role_schema};

use crate::config::ServerConfig;

const MAX_CACHED_ROLES: usize = 1024;

/// Everything derived from one metadata version.
pub struct AppState {
    pub metadata: Arc<Metadata>,
    pub tables: Arc<Tables>,
    pub sources: HashMap<String, Arc<PgSource>>,
    pub inconsistencies: Vec<String>,
    pub options: SchemaOptions,
    schemas: RwLock<HashMap<RoleName, Arc<RoleSchema>>>,
}

impl AppState {
    pub fn new(
        metadata: Metadata,
        tables: Tables,
        sources: HashMap<String, Arc<PgSource>>,
        inconsistencies: Vec<String>,
        options: SchemaOptions,
    ) -> Self {
        let state = AppState {
            metadata: Arc::new(metadata),
            tables: Arc::new(tables),
            sources,
            inconsistencies,
            options,
            schemas: RwLock::new(HashMap::new()),
        };
        // Build the admin schema eagerly so the first request is fast and errors surface early.
        state.role_schema(&RoleName::admin());
        state
    }

    /// The schema for a role, built on first use and cached for this metadata version.
    pub fn role_schema(&self, role: &RoleName) -> Arc<RoleSchema> {
        if let Some(s) = self.schemas.read().expect("lock").get(role) {
            return s.clone();
        }
        let built = Arc::new(build_role_schema(&self.tables, role, &self.options));
        let mut schemas = self.schemas.write().expect("lock");
        // Roles come from request headers/tokens; bound the cache so it cannot grow forever.
        if schemas.len() >= MAX_CACHED_ROLES && !schemas.contains_key(role) {
            return built;
        }
        schemas.entry(role.clone()).or_insert(built).clone()
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
