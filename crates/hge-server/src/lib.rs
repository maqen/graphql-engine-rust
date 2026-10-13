//! The HTTP server: routes, request pipeline, state management.

pub mod action_log;
pub mod config;
pub mod events;
pub mod graphql;
pub mod load;
pub mod migrations;
pub mod routes;
pub mod scheduled;
pub mod state;
pub mod sync;
pub mod v1_query;
pub mod ws;

use std::sync::Arc;

use hge_metadata::store::{MemoryStore, MetadataStore};
use hge_metadata_api::{PgCatalog, Reload};

pub use config::ServerConfig;
pub use state::{AppState, Engine};

/// Initialise the metadata catalog, load metadata, connect sources and build the engine.
pub async fn build_engine(config: ServerConfig) -> Result<Arc<Engine>, hge_core::Error> {
    // Auth first: its configuration errors are fatal before anything connects (as in V2).
    let auth = hge_auth::Authenticator::new(config.auth_config())
        .await
        .map_err(|e| hge_core::Error::new(hge_core::Code::InvalidConfiguration, e))?;
    let store: Arc<dyn MetadataStore> = match config.catalog_url() {
        Some(url) => {
            let catalog = PgCatalog::connect(url)?
                .with_extensions_schema(&config.metadata_database_extensions_schema);
            let outcome = catalog.init().await?;
            tracing::info!(target: "startup", ?outcome, "metadata catalog ready");
            Arc::new(catalog)
        }
        None => Arc::new(MemoryStore::default()),
    };
    let (metadata, resource_version, origin) = load::load_metadata(&config, store.as_ref()).await?;
    tracing::info!(target: "startup", ?origin, resource_version, "metadata source selected");
    if let Some(dir) = &config.migrations_dir {
        migrations::apply(dir, &metadata, &config).await?;
    }
    let prepared = load::prepare(&metadata, &config, None, &Reload::All, Default::default()).await;
    load::log_prepared(&prepared);
    let options = load::schema_options(&config);
    let action_log = match config.catalog_url() {
        Some(url) => {
            let store: Arc<dyn hge_actions::ActionLogStore> = Arc::new(
                action_log::PgActionLog::connect(url, &action_log_pool(&config))?,
            );
            Some(store)
        }
        None => None,
    };
    let state = AppState::new(metadata, prepared, resource_version, options);
    let mut engine = Engine::new(config, auth, store, state);
    engine.action_log = action_log;
    Ok(Arc::new(engine))
}

/// The metadata database pool of the action log: one connection per row of a batch (the
/// processor runs them concurrently) plus a few for async mutations and queries.
fn action_log_pool(config: &ServerConfig) -> hge_postgres::PoolConfig {
    hge_postgres::PoolConfig {
        max_connections: config.async_actions_fetch_batch_size.max(5) as usize + 4,
        use_prepared_statements: config.use_prepared_statements,
        // Fail a request (or a processor pass) instead of waiting forever for a connection.
        pool_timeout: Some(60),
        ..hge_postgres::PoolConfig::default()
    }
}

/// How long in-flight HTTP requests get on shutdown (V2: warp's fixed 30 s, spec 11),
/// independently of `HASURA_GRAPHQL_GRACEFUL_SHUTDOWN_TIMEOUT`.
const HTTP_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Start the async action processor; it stops when `shutdown` is cancelled. `None` without a
/// metadata database.
pub fn start_async_processor(
    engine: &Arc<Engine>,
    shutdown: tokio_util::sync::CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    let store = engine.action_log.clone()?;
    let config = hge_actions::ProcessorConfig {
        fetch_interval: engine.config.async_actions_fetch_interval,
        batch_size: engine.config.async_actions_fetch_batch_size,
        graceful_shutdown_timeout: engine.config.events.graceful_shutdown_timeout,
    };
    let weak = Arc::downgrade(engine);
    let registry = move || match weak.upgrade() {
        Some(engine) => engine.state.load().action_registry.clone(),
        // The engine is gone: nothing to process (the loop is being shut down).
        None => Arc::new(hge_actions::ActionRegistry::empty(
            Arc::new(hge_actions::ProcessEnv),
            hge_actions::HandlerConfig::default(),
        )),
    };
    Some(tokio::spawn(hge_actions::async_action::run_processor(
        registry, store, config, shutdown,
    )))
}

/// Serve until ctrl-c / SIGTERM, running schema sync, the event trigger worker and the async
/// action processor alongside. On the signal, the workers start their graceful shutdown while
/// HTTP requests drain (at most [`HTTP_DRAIN_TIMEOUT`]).
pub async fn serve(engine: Arc<Engine>) -> std::io::Result<()> {
    let addr = format!("{}:{}", engine.config.host, engine.config.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(target: "startup", %addr, "server listening");
    sync::spawn(engine.clone());
    let shutdown = tokio_util::sync::CancellationToken::new();
    let worker = events::spawn_worker(&engine, shutdown.clone());
    let scheduler = scheduled::spawn_worker(&engine, shutdown.clone());
    let processor = start_async_processor(&engine, shutdown.clone());
    let signal = {
        let shutdown = shutdown.clone();
        let engine = engine.clone();
        async move {
            shutdown_signal().await;
            // Upgraded connections are not tracked by the HTTP server: close them (1012, so
            // clients reconnect to another instance) before it stops.
            engine.ws.shutdown(std::time::Duration::from_secs(5)).await;
            shutdown.cancel();
        }
    };
    let server = axum::serve(listener, routes::router(engine)).with_graceful_shutdown(signal);
    let drain_deadline = async {
        shutdown.cancelled().await;
        tokio::time::sleep(HTTP_DRAIN_TIMEOUT).await;
    };
    let served = tokio::select! {
        served = server => served,
        _ = drain_deadline => {
            tracing::warn!(target: "shutdown", "in-flight HTTP requests did not finish in time");
            Ok(())
        }
    };
    // Also stop the workers when the server exits for another reason.
    shutdown.cancel();
    for task in [worker, scheduler, processor].into_iter().flatten() {
        let _ = task.await;
    }
    served
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
    tracing::info!(target: "shutdown", "shutting down");
}
