//! The working state of one metadata request.
//!
//! A request takes the instance's metadata lock, reads the stored metadata, and applies its
//! command(s) to a working copy. Each modifying command is followed by a [`Ctx::check`]: the
//! working copy is resolved against the databases and new inconsistencies are rejected as V2
//! does. Only when everything succeeded is the result written to the store (with V2's
//! optimistic `resource_version` check), announced to other instances, and swapped into the
//! running server.

use hge_core::{Code, InconsistentObject, Session};
use hge_events::scheduled_admin::{CronEventChange, apply_cron_changes};
use hge_metadata::Metadata;
use hge_metadata::store::Invalidations;
use serde_json::Value;
use tokio::sync::MutexGuard;

use crate::response::{ApiError, ApiResult};
use crate::runtime::{PrepareOptions, Reload, Runtime};

/// How a modifying command treats inconsistencies of the resulting metadata.
#[derive(Debug, Clone)]
pub enum Check {
    /// Store whatever comes out (`replace_metadata` with `allow_inconsistent_metadata`,
    /// `reload_metadata`).
    Allow,
    /// Reject any inconsistency at all (`replace_metadata`).
    None,
    /// Reject inconsistencies that were not there before.
    NoNew,
    /// Like [`Check::NoNew`], but if the object being created is itself inconsistent, report
    /// it as `invalid-configuration` (V2's `buildSchemaCacheFor`).
    Object { kind: String, name: String },
}

impl Check {
    pub fn object(kind: &str, name: impl Into<String>) -> Self {
        Check::Object {
            kind: kind.to_owned(),
            name: name.into(),
        }
    }
}

pub struct Ctx<'a, R: Runtime> {
    pub rt: &'a R,
    /// The caller (an admin), recorded with events and SQL the request causes.
    pub session: &'a Session,
    _guard: Option<MutexGuard<'a, ()>>,
    /// The working copy.
    pub metadata: Metadata,
    /// The stored resource version this request started from.
    pub resource_version: i64,
    /// The resolved state of `metadata` (after the last check), if any.
    pub prepared: Option<R::Prepared>,
    /// Inconsistencies of the last resolved state.
    pub inconsistencies: Vec<InconsistentObject>,
    /// Whether the metadata must be written back.
    pub modified: bool,
    /// Bump the resource version even when the metadata is unchanged (`reload_metadata`).
    pub bump: bool,
    /// Sources to reconnect on the next check.
    pub reload: Reload,
    pub invalidations: Invalidations,
    pub warnings: Vec<Value>,
    /// Changes to stored cron events, applied after the metadata is committed.
    pub cron_changes: Vec<CronEventChange>,
}

/// A copy of the parts of [`Ctx`] a failed `bulk_keep_going` item rolls back.
pub struct Snapshot<P> {
    metadata: Metadata,
    prepared: Option<P>,
    inconsistencies: Vec<InconsistentObject>,
    modified: bool,
    bump: bool,
    reload: Reload,
    invalidations: Invalidations,
    warnings: usize,
    cron_changes: usize,
}

impl<'a, R: Runtime> Ctx<'a, R> {
    /// Start a request: take the lock (for requests that may write) and read the stored
    /// metadata, falling back to the installed metadata if nothing is stored.
    pub async fn begin(rt: &'a R, session: &'a Session, lock: bool) -> ApiResult<Self> {
        let guard = if lock { Some(rt.lock().await) } else { None };
        let installed = rt.installed();
        let (metadata, resource_version) = match rt.store().fetch().await? {
            Some(stored) => {
                let md = if stored.resource_version == installed.resource_version {
                    (*installed.metadata).clone()
                } else {
                    hge_metadata::from_json(stored.metadata)?
                };
                (md, stored.resource_version)
            }
            None => ((*installed.metadata).clone(), 0),
        };
        // Another instance changed the metadata since we installed ours: resolve it first so
        // "new" inconsistencies are judged against the right baseline.
        let (prepared, inconsistencies) = if resource_version != installed.resource_version {
            let p = rt.prepare(&metadata, PrepareOptions::default()).await;
            let inc = rt.inconsistencies(&p).to_vec();
            (Some(p), inc)
        } else {
            (None, installed.inconsistencies)
        };
        Ok(Ctx {
            rt,
            session,
            _guard: guard,
            metadata,
            resource_version,
            prepared,
            inconsistencies,
            modified: false,
            bump: false,
            reload: Reload::Changed,
            invalidations: Invalidations::default(),
            warnings: vec![],
            cron_changes: vec![],
        })
    }

    pub fn snapshot(&self) -> Snapshot<R::Prepared> {
        Snapshot {
            metadata: self.metadata.clone(),
            prepared: self.prepared.clone(),
            inconsistencies: self.inconsistencies.clone(),
            modified: self.modified,
            bump: self.bump,
            reload: self.reload.clone(),
            invalidations: self.invalidations.clone(),
            warnings: self.warnings.len(),
            cron_changes: self.cron_changes.len(),
        }
    }

    pub fn restore(&mut self, s: Snapshot<R::Prepared>) {
        self.metadata = s.metadata;
        self.prepared = s.prepared;
        self.inconsistencies = s.inconsistencies;
        self.modified = s.modified;
        self.bump = s.bump;
        self.reload = s.reload;
        self.invalidations = s.invalidations;
        self.warnings.truncate(s.warnings);
        self.cron_changes.truncate(s.cron_changes);
    }

    /// Resolve the working copy and apply `check`. On success it becomes the new baseline.
    pub async fn check(&mut self, check: Check) -> ApiResult<()> {
        self.check_with(check, Default::default()).await
    }

    pub async fn check_with(
        &mut self,
        check: Check,
        catalogs: std::collections::HashMap<String, hge_core::SourceCatalog>,
    ) -> ApiResult<()> {
        self.modified = true;
        self.invalidations.metadata = true;
        let reload = std::mem::take(&mut self.reload);
        let prepared = self
            .rt
            .prepare(
                &self.metadata,
                PrepareOptions {
                    base: self.prepared.as_ref(),
                    reload,
                    catalogs,
                },
            )
            .await;
        let found = self.rt.inconsistencies(&prepared).to_vec();
        let new: Vec<&InconsistentObject> = found
            .iter()
            .filter(|o| !self.inconsistencies.iter().any(|b| b.id() == o.id()))
            .collect();
        match &check {
            Check::Allow => {}
            Check::None if !found.is_empty() => {
                return Err(inconsistent(
                    "cannot continue due to inconsistent metadata",
                    &found.iter().collect::<Vec<_>>(),
                ));
            }
            Check::None => {}
            Check::Object { kind, name } => {
                let own: Vec<&InconsistentObject> = new
                    .iter()
                    .copied()
                    .filter(|o| o.kind == *kind && o.name == *name)
                    .collect();
                if !own.is_empty() {
                    let reasons: Vec<String> = own
                        .iter()
                        .map(|o| format!("Inconsistent object: {}", o.reason))
                        .collect();
                    return Err(
                        ApiError::bad(Code::InvalidConfiguration, reasons.join(", "))
                            .with_internal(objects_json(&own)),
                    );
                }
                if !new.is_empty() {
                    return Err(inconsistent(
                        "cannot continue due to new inconsistent metadata",
                        &new,
                    ));
                }
            }
            Check::NoNew if !new.is_empty() => {
                return Err(inconsistent(
                    "cannot continue due to new inconsistent metadata",
                    &new,
                ));
            }
            Check::NoNew => {}
        }
        self.prepared = Some(prepared);
        self.inconsistencies = found;
        Ok(())
    }

    /// Re-resolve the unchanged metadata (reconnecting `reload`) and bump the resource
    /// version on commit, like `reload_metadata`.
    pub async fn refresh(&mut self, reload: Reload) {
        self.reload.merge(reload);
        let reload = std::mem::take(&mut self.reload);
        if let Reload::Sources(s) = &reload {
            self.invalidations.sources.extend(s.iter().cloned());
        }
        self.invalidations.metadata = true;
        let prepared = self
            .rt
            .prepare(
                &self.metadata,
                PrepareOptions {
                    base: self.prepared.as_ref(),
                    reload,
                    catalogs: Default::default(),
                },
            )
            .await;
        self.inconsistencies = self.rt.inconsistencies(&prepared).to_vec();
        self.prepared = Some(prepared);
        self.bump = true;
    }

    /// Mark the metadata changed and (re)resolve it with [`Check::NoNew`].
    pub async fn changed(&mut self) -> ApiResult<()> {
        self.check(Check::NoNew).await
    }

    /// Reject a client-supplied `resource_version` that is not the one this unit of work is
    /// based on (V2's optimistic concurrency check for modifying requests).
    pub fn expect_version(&self, requested: Option<i64>) -> ApiResult<()> {
        match requested {
            Some(v) if v != self.resource_version => Err(hge_metadata::store::conflict(v).into()),
            _ => Ok(()),
        }
    }

    /// Give up on the working copy. Resolving it may already have changed the sources (event
    /// trigger SQL is installed when metadata is resolved), so the stored metadata is
    /// resolved again to put them back.
    pub async fn discard(self) {
        if !self.modified {
            return;
        }
        let installed = self.rt.installed();
        let stored = match self.rt.store().fetch().await {
            Ok(Some(s)) if s.resource_version != installed.resource_version => {
                hge_metadata::from_json(s.metadata).ok()
            }
            _ => None,
        };
        let metadata = stored.as_ref().unwrap_or(&installed.metadata);
        let _ = self.rt.prepare(metadata, PrepareOptions::default()).await;
    }

    /// Write the result back and install it. The store only accepts the write if nobody else
    /// wrote since [`Ctx::begin`]. Returns the new resource version (unchanged when nothing
    /// was modified).
    pub async fn commit(mut self) -> ApiResult<i64> {
        if !self.modified && !self.bump {
            return Ok(self.resource_version);
        }
        if self.rt.settings().read_only {
            return Err(ApiError::bad(
                Code::NotSupported,
                "metadata cannot be modified in read-only mode",
            ));
        }
        let prepared = match self.prepared.take() {
            Some(p) if self.reload == Reload::Changed => p,
            base => {
                let reload = std::mem::take(&mut self.reload);
                self.rt
                    .prepare(
                        &self.metadata,
                        PrepareOptions {
                            base: base.as_ref(),
                            reload,
                            catalogs: Default::default(),
                        },
                    )
                    .await
            }
        };
        let store = self.rt.store();
        let stored = if self.modified {
            let doc = hge_metadata::export_json(&self.metadata);
            store.store(&doc, self.resource_version).await
        } else {
            store.bump_version(self.resource_version).await
        };
        let version = match stored {
            Ok(v) => v,
            Err(e) => {
                self.discard().await;
                return Err(e.into());
            }
        };
        // Install before anything else can fail or be cancelled: the store already has it.
        self.rt.install(prepared, self.metadata, version);
        if let (Some(pool), false) = (self.rt.metadata_db(), self.cron_changes.is_empty()) {
            // On failure the scheduled-event worker tops events up and drops stale ones.
            if let Err(e) = apply_cron_changes(pool, &self.cron_changes).await {
                tracing::warn!(target: "metadata", "could not update cron events: {e}");
            }
        }
        if let Err(e) = store
            .notify(version, &self.invalidations, self.rt.instance_id())
            .await
        {
            tracing::warn!(target: "metadata", "could not write schema sync notification: {e}");
        }
        Ok(version)
    }
}

fn objects_json(objects: &[&InconsistentObject]) -> Value {
    Value::Array(objects.iter().map(|o| o.to_json()).collect())
}

fn inconsistent(message: &str, objects: &[&InconsistentObject]) -> ApiError {
    ApiError::bad(Code::Unexpected, message).with_internal(objects_json(objects))
}
