//! The HTTP server: routes, request pipeline, state management.

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
            let catalog = PgCatalog::connect(url)?;
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
    let state = AppState::new(metadata, prepared, resource_version, options);
    Ok(Arc::new(Engine::new(config, auth, store, state)))
}

/// Serve until ctrl-c / SIGTERM, running schema sync and the event trigger worker alongside.
pub async fn serve(engine: Arc<Engine>) -> std::io::Result<()> {
    let addr = format!("{}:{}", engine.config.host, engine.config.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(target: "startup", %addr, "server listening");
    sync::spawn(engine.clone());
    let shutdown = tokio_util::sync::CancellationToken::new();
    let worker = events::spawn_worker(&engine, shutdown.clone());
    let scheduler = scheduled::spawn_worker(&engine, shutdown.clone());
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
    let served = axum::serve(listener, routes::router(engine))
        .with_graceful_shutdown(signal)
        .await;
    // Also stop the worker when the server exits for another reason.
    shutdown.cancel();
    for handle in [worker, scheduler].into_iter().flatten() {
        let _ = handle.await;
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
