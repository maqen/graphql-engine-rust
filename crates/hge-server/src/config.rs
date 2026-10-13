//! Server configuration (the typed form of V2's `graphql-engine serve` flags / env vars).

use std::path::PathBuf;

pub use hge_events::EventingConfig;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    /// `HASURA_GRAPHQL_DATABASE_URL`
    pub database_url: Option<String>,
    /// `HASURA_GRAPHQL_METADATA_DATABASE_URL`
    pub metadata_database_url: Option<String>,
    /// `HASURA_GRAPHQL_METADATA_DIR` (as used by the `cli-migrations` image).
    pub metadata_dir: Option<PathBuf>,
    /// `HASURA_GRAPHQL_MIGRATIONS_DIR`: applied on boot (as the `cli-migrations` image does).
    pub migrations_dir: Option<PathBuf>,
    /// `HASURA_GRAPHQL_ADMIN_SECRET` (+ `HASURA_GRAPHQL_ADMIN_SECRETS`).
    pub admin_secrets: Vec<String>,
    /// `HASURA_GRAPHQL_UNAUTHORIZED_ROLE`
    pub unauthorized_role: Option<String>,
    /// `HASURA_GRAPHQL_DISABLE_ADMIN_SECRET`
    pub disable_admin_secret: bool,
    /// `HASURA_GRAPHQL_JWT_SECRET` (a JSON object, validated by `hge-auth`).
    pub jwt_secret: Option<String>,
    /// `HASURA_GRAPHQL_JWT_SECRETS` (a JSON list).
    pub jwt_secrets: Option<String>,
    /// `HASURA_GRAPHQL_AUTH_HOOK*`
    pub auth_hook: Option<hge_auth::HookConfig>,
    /// `HASURA_GRAPHQL_DEV_MODE`
    pub dev_mode: bool,
    /// `HASURA_GRAPHQL_ADMIN_INTERNAL_ERRORS` (default true)
    pub admin_internal_errors: bool,
    /// `HASURA_GRAPHQL_STRINGIFY_NUMERIC_TYPES`
    pub stringify_numeric_types: bool,
    /// `HASURA_GRAPHQL_V1_BOOLEAN_NULL_COLLAPSE`
    pub v1_boolean_null_collapse: bool,
    /// `HASURA_GRAPHQL_CORS_DOMAIN` (`*` or comma-separated origins)
    pub cors_domain: String,
    /// `HASURA_GRAPHQL_DISABLE_CORS`
    pub disable_cors: bool,
    /// `HASURA_GRAPHQL_ENABLED_APIS` (`graphql,metadata,config,pgdump,metrics`)
    pub enabled_apis: Vec<String>,
    /// `HASURA_GRAPHQL_PG_CONNECTIONS`: default pool size for sources without `pool_settings`.
    pub pg_connections: usize,
    /// `HASURA_GRAPHQL_USE_PREPARED_STATEMENTS`
    pub use_prepared_statements: bool,
    /// `HASURA_GRAPHQL_PRESERVE_401_ERRORS`
    pub preserve_401_errors: bool,
    /// Event trigger processing (`HASURA_GRAPHQL_EVENTS_*`, `DISABLE_EVENTING`, …).
    pub events: hge_events::EventingConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            host: "0.0.0.0".to_owned(),
            port: 8080,
            database_url: None,
            metadata_database_url: None,
            metadata_dir: None,
            migrations_dir: None,
            admin_secrets: vec![],
            unauthorized_role: None,
            disable_admin_secret: false,
            jwt_secret: None,
            jwt_secrets: None,
            auth_hook: None,
            dev_mode: false,
            admin_internal_errors: true,
            stringify_numeric_types: false,
            v1_boolean_null_collapse: false,
            cors_domain: "*".to_owned(),
            disable_cors: false,
            enabled_apis: vec!["graphql".into(), "metadata".into()],
            pg_connections: 50,
            use_prepared_statements: true,
            preserve_401_errors: false,
            events: hge_events::EventingConfig {
                user_agent: format!("hasura-graphql-engine/{}", crate::routes::VERSION),
                ..hge_events::EventingConfig::default()
            },
        }
    }
}

impl ServerConfig {
    /// The auth settings, for [`hge_auth::Authenticator::new`].
    pub fn auth_config(&self) -> hge_auth::AuthConfig {
        hge_auth::AuthConfig {
            admin_secrets: self.admin_secrets.clone(),
            unauthorized_role: self.unauthorized_role.clone(),
            disable_admin_secret: self.disable_admin_secret,
            jwt_secret: self.jwt_secret.clone(),
            jwt_secrets: self.jwt_secrets.clone(),
            auth_hook: self.auth_hook.clone(),
            user_agent: format!("hasura-graphql-engine/{}", crate::routes::VERSION),
        }
    }

    pub fn api_enabled(&self, api: &str) -> bool {
        self.enabled_apis.iter().any(|a| a == api)
    }

    /// The database holding `hdb_catalog` (V2: metadata DB url, else the database url).
    pub fn catalog_url(&self) -> Option<&str> {
        self.metadata_database_url
            .as_deref()
            .or(self.database_url.as_deref())
    }
}
