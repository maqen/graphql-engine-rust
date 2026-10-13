//! Subscriptions to asynchronous actions' results, `<action>(id:) { … }` (spec 06 §9.5,
//! spec 09 §9).
//!
//! V2 polls these from one global loop keyed by the client's operation id. Here each
//! subscription is a task owned by its connection (so `stop` and closing the socket end it,
//! which V2 does not do) that reads the action log every second and sends the result only
//! when it changed. The two V2 modes:
//!
//! - output type without relationships: once every action has finished, the final result
//!   is followed by `complete` (V2 resends the result every second until then);
//! - output type with relationships: the result is a live query (V2 runs it as one over the
//!   action log), so it keeps running, and the relationships are re-read, until stopped.
//!
//! An error (such as an unknown action id) is sent as an `error` message and polling goes
//! on, as V2 does when building the response fails; V2 instead drops the operation when
//! reading the log fails, which here is retried too.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::http::HeaderMap;
use hge_core::Session;
use hge_ir::ActionRootField;
use hge_subscriptions::protocol::{self, Protocol};
use serde_json::{Map, Value as Json};
use tokio::sync::{Semaphore, mpsc};

use super::{Endpoint, Event, Outbox, error_json, error_message};
use crate::graphql::{self, StateSources};
use crate::state::{AppState, Engine};

/// How often the action log is read (V2: 1 s).
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// At most this many subscriptions read the action log at once, engine-wide, so many
/// subscribers cannot exhaust the metadata database's pool (V2 reads them one at a time).
pub(super) const CONCURRENT_POLLS: usize = 8;

/// The connection's handle on a subscription task: dropping it stops the task, and nothing
/// is sent for the operation afterwards.
pub(super) struct TaskHandle {
    task: tokio::task::AbortHandle,
    active: Arc<AtomicBool>,
    released: Arc<AtomicBool>,
}

impl TaskHandle {
    /// The task completed and sent its last message, so its operation id is free (it may
    /// not have reported it yet). Not after an overflow: the connection must still send
    /// that error.
    pub fn is_released(&self) -> bool {
        self.released.load(Ordering::SeqCst)
    }
}

impl Drop for TaskHandle {
    fn drop(&mut self) {
        // The task may be between two awaits on another thread: the flag stops its sends.
        self.active.store(false, Ordering::SeqCst);
        self.task.abort();
    }
}

pub(super) struct AsyncActionSubscription {
    pub engine: Arc<Engine>,
    pub state: Arc<AppState>,
    pub session: Session,
    pub headers: Arc<HeaderMap>,
    pub query: String,
    /// Root fields by response key.
    pub fields: Vec<(String, ActionRootField)>,
    /// Some output type has relationships: run as a live query (never `complete`).
    pub live: bool,
    pub id: String,
    pub generation: u64,
    pub protocol: Protocol,
    pub endpoint: Endpoint,
    pub out: Outbox,
    pub events: mpsc::UnboundedSender<Event>,
    pub polls: Arc<Semaphore>,
}

impl AsyncActionSubscription {
    pub fn spawn(self) -> TaskHandle {
        let active = Arc::new(AtomicBool::new(true));
        let released = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(self.run(active.clone(), released.clone())).abort_handle();
        TaskHandle {
            task,
            active,
            released,
        }
    }

    async fn run(self, active: Arc<AtomicBool>, released: Arc<AtomicBool>) {
        let include_internal = graphql::include_internal(&self.engine, &self.session);
        let mut last: Option<String> = None;
        loop {
            let ctx = hge_actions::RequestContext {
                session: &self.session,
                client_headers: &self.headers,
                query: Some(&self.query),
                action_log: self.engine.action_log.as_deref(),
                include_internal,
            };
            let sources = StateSources(&self.state);
            let polls = {
                let _permit = self
                    .polls
                    .acquire()
                    .await
                    .expect("semaphore is never closed");
                futures::future::join_all(
                    self.fields
                        .iter()
                        .map(|(_, field)| hge_actions::poll(field, &ctx, &sources)),
                )
                .await
            };
            if !active.load(Ordering::SeqCst) {
                return;
            }
            let mut data = Map::new();
            let mut finished = true;
            let mut error = None;
            for ((alias, _), poll) in self.fields.iter().zip(polls) {
                match poll {
                    Ok(p) => {
                        finished &= p.finished;
                        data.insert(alias.clone(), p.value);
                    }
                    Err(e) => {
                        error = Some(e);
                        break;
                    }
                }
            }
            if let Some(e) = error {
                let message = error_message(
                    self.protocol,
                    self.endpoint,
                    &self.id,
                    error_json(&e, self.endpoint),
                );
                if last.as_ref() != Some(&message) {
                    self.out.control(message.clone());
                    last = Some(message);
                }
            } else {
                let body = serde_json::json!({ "data": Json::Object(data) }).to_string();
                if last.as_ref() != Some(&body) {
                    if !self.out.data(self.protocol.data(&self.id, &body)) {
                        let _ = self.events.send(Event::Overflow(
                            self.id.clone(),
                            self.generation,
                            None,
                        ));
                        return;
                    }
                    last = Some(body);
                }
                if finished && !self.live {
                    self.out.control(protocol::complete(&self.id));
                    released.store(true, Ordering::SeqCst);
                    let _ = self
                        .events
                        .send(Event::Finished(self.id.clone(), self.generation));
                    return;
                }
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}
