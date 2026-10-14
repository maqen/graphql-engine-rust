//! Multi-instance metadata sync, like V2's schema-sync listener and processor.
//!
//! Every `HASURA_GRAPHQL_SCHEMA_SYNC_POLL_INTERVAL` ms the stored `resource_version` is
//! compared with the installed one; when another instance (ours or V2) changed the metadata,
//! it is fetched, resolved and swapped in. A single cheap query per tick; the rebuild happens
//! only on change and never blocks requests (they keep using the old state until the swap).

use std::sync::Arc;
use std::time::Duration;

use hge_metadata_api::{PrepareOptions, Reload, RemoteReload, Runtime};
use tracing::{info, warn};

use crate::state::Engine;

/// Start the background poller (no-op when the interval is 0).
pub fn spawn(engine: Arc<Engine>) {
    let interval = engine.config.schema_sync_poll_interval_ms;
    if interval == 0 {
        return;
    }
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_millis(interval));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if let Err(e) = sync_once(&engine).await {
                warn!(target: "schema-sync", "schema sync failed: {e}");
            }
        }
    });
}

/// Reload the metadata if the store has a newer version. Returns whether it did.
pub async fn sync_once(engine: &Engine) -> Result<bool, hge_core::Error> {
    let stored = engine.store.resource_version().await?;
    if stored <= engine.state.load().resource_version {
        return Ok(false);
    }
    let _guard = engine.lock().await;
    let installed = engine.state.load().resource_version;
    let Some(fresh) = engine.store.fetch().await? else {
        return Ok(false);
    };
    if fresh.resource_version <= installed {
        return Ok(false);
    }
    // Sources and remote schemas the other instance asked to reload (e.g. reload_metadata).
    let (reload, remote_schemas) = match engine
        .store
        .notification_since(installed, &engine.instance_id)
        .await
    {
        Ok(Some((version, inv))) => {
            let reload = if inv.sources.is_empty() {
                Reload::Introspect
            } else {
                Reload::Sources(inv.sources.into_iter().collect())
            };
            // Only the latest notification is kept: when we missed versions, a remote
            // schema reload among them is lost, so re-introspect them all.
            let remotes = if version == fresh.resource_version && version == installed + 1 {
                RemoteReload::Only(inv.remote_schemas.into_iter().collect())
            } else {
                RemoteReload::All
            };
            (reload, remotes)
        }
        _ => (Reload::Introspect, RemoteReload::All),
    };
    let metadata = hge_metadata::from_json(fresh.metadata)?;
    let prepared = engine
        .prepare(
            &metadata,
            PrepareOptions {
                base: None,
                reload,
                remote_schemas,
                catalogs: Default::default(),
            },
        )
        .await;
    info!(
        target: "schema-sync",
        from = installed,
        to = fresh.resource_version,
        "metadata changed by another instance; reloaded"
    );
    engine.install(prepared, metadata, fresh.resource_version);
    Ok(true)
}
