//! The HTTP server: routes, request pipeline, state management.

pub mod config;
pub mod graphql;
pub mod load;
pub mod routes;
pub mod state;

use std::sync::Arc;

pub use config::ServerConfig;
pub use state::{AppState, Engine};

/// Load metadata, connect sources and build the engine.
pub async fn build_engine(config: ServerConfig) -> Result<Arc<Engine>, hge_core::Error> {
    let (metadata, origin) = load::load_metadata(&config).await?;
    tracing::info!(target: "startup", ?origin, "metadata source selected");
    let state = load::build_state(metadata, &config).await;
    Ok(Arc::new(Engine::new(config, state)))
}

/// Serve until ctrl-c / SIGTERM.
pub async fn serve(engine: Arc<Engine>) -> std::io::Result<()> {
    let addr = format!("{}:{}", engine.config.host, engine.config.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(target: "startup", %addr, "server listening");
    axum::serve(listener, routes::router(engine))
        .with_graceful_shutdown(shutdown_signal())
        .await
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
