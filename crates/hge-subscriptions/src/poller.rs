//! Multiplexed polling (spec 09 §6, §7).
//!
//! * A **poller** runs one multiplexed statement ([`MultiplexedQuery::sql`]) on one source.
//!   Every subscription with the same statement text shares it, whatever its role: roles,
//!   permissions and arguments that differ change either the text or the values.
//! * A **cohort** is the set of subscribers of a poller whose values
//!   ([`MultiplexedQuery::variables`]) are identical; one cohort is one row of the statement's
//!   result and its subscribers share that result.
//! * Every refetch interval the poller runs its statement for all cohorts, `batch_size`
//!   cohorts per round trip, batches concurrently.
//!
//! Differences from V2, none of them visible on the wire except as lower latency:
//!
//! * The first result is fetched when a subscription starts (which also validates it, as
//!   V2's test run does) and sent at once, instead of waiting for the poller's next round.
//! * Each subscriber remembers the hash of the last result it was sent, so it never gets a
//!   duplicate and never gets an older result than one it has seen.
//! * A streaming cohort's cursor is one of its values; advancing it re-keys the cohort, so
//!   subscribers that reach the same cursor position share a cohort again.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use hge_core::Error;
use hge_ir::{MultiplexedQuery, MultiplexedRow, SourceExecutor};
use xxhash_rust::xxh3::Xxh3;

/// Poller settings (`HASURA_GRAPHQL_LIVE_QUERIES_MULTIPLEXED_*`,
/// `HASURA_GRAPHQL_STREAMING_QUERIES_MULTIPLEXED_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollerOptions {
    /// Sleep between two polls of a poller.
    pub refetch_interval: Duration,
    /// Cohorts per statement execution (at least 1).
    pub batch_size: usize,
}

impl Default for PollerOptions {
    fn default() -> Self {
        PollerOptions {
            refetch_interval: Duration::from_secs(1),
            batch_size: 100,
        }
    }
}

/// Receives the results of one subscription. Called while the poller's state is locked:
/// implementations must not block or call back into the registry (queue the message).
pub trait Subscriber: Send + Sync {
    /// A new result: one JSON document per root field (`None` is `null`). For a stream,
    /// `cursor` is the cursor value the batch was fetched from (to resume from if the batch
    /// cannot be delivered).
    fn on_data(&self, fields: &[Option<String>], cursor: Option<&str>);
    /// The poll failed; the subscription stays active.
    fn on_error(&self, error: &Error);
}

type Values = Arc<[Option<String>]>;

#[derive(Clone, PartialEq, Eq, Hash)]
struct PollerKey {
    /// Address of the executor (kept alive by the poller, so it cannot be reused).
    executor: usize,
    sql: Arc<str>,
}

/// All pollers of one kind (live queries or streaming).
pub struct Registry {
    inner: Arc<RegistryInner>,
}

struct RegistryInner {
    options: PollerOptions,
    pollers: Mutex<HashMap<PollerKey, Arc<Poller>>>,
    next_id: AtomicU64,
}

struct Poller {
    key: PollerKey,
    executor: Arc<dyn SourceExecutor>,
    /// Streaming: index of the cursor among the values.
    cursor: Option<usize>,
    state: Mutex<PollerState>,
    task: Mutex<Option<tokio::task::AbortHandle>>,
}

#[derive(Default)]
struct PollerState {
    cohorts: HashMap<Values, Cohort>,
    /// Which cohort each subscriber is in.
    members: HashMap<u64, Values>,
    /// Incremented by every poll; subscribers that joined after a poll took its snapshot
    /// (`joined > epoch`) are left out of that poll's results.
    epoch: u64,
    /// The last subscriber left and the poller is stopping: `add` must use a new poller.
    closed: bool,
}

struct Cohort {
    /// The values as the JSON array sent to the database.
    json: Arc<str>,
    /// Hash of the last result (live queries).
    hash: Option<u128>,
    /// Some subscriber was not sent the last result (it joined later or after an error).
    mixed: bool,
    subscribers: HashMap<u64, Member>,
}

struct Member {
    sink: Arc<dyn Subscriber>,
    last_hash: Option<u128>,
    joined: u64,
}

/// Keeps a subscription registered; dropping it unsubscribes.
pub struct SubscriptionHandle {
    registry: Weak<RegistryInner>,
    poller: Arc<Poller>,
    id: u64,
}

impl Drop for SubscriptionHandle {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade() {
            registry.remove(&self.poller, self.id);
        }
    }
}

impl Registry {
    pub fn new(options: PollerOptions) -> Self {
        let options = PollerOptions {
            batch_size: options.batch_size.max(1),
            ..options
        };
        Registry {
            inner: Arc::new(RegistryInner {
                options,
                pollers: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
            }),
        }
    }

    pub fn options(&self) -> PollerOptions {
        self.inner.options
    }

    /// Start a subscription: run it once for `sink` (errors here are the subscription's start
    /// errors), deliver the first result, then add it to its poller and cohort.
    pub async fn subscribe(
        &self,
        executor: Arc<dyn SourceExecutor>,
        query: MultiplexedQuery,
        sink: Arc<dyn Subscriber>,
    ) -> Result<SubscriptionHandle, Error> {
        let MultiplexedQuery {
            sql,
            mut variables,
            cursor,
        } = query;
        let json = values_json(&variables);
        let rows = executor
            .execute_multiplexed(&sql, &format!("[{json}]"))
            .await?;
        let row = rows
            .into_iter()
            .find(|r| r.index == 0)
            .ok_or_else(|| Error::unexpected("subscription query returned no result"))?;
        let (fields, next_cursor) = split_cursor(row, cursor.is_some());
        let last_hash = match cursor {
            None => {
                sink.on_data(&fields, None);
                Some(hash(&fields))
            }
            Some(i) => {
                if !is_empty_batch(&fields) {
                    sink.on_data(&fields, variables[i].as_deref());
                }
                if let Some(c) = next_cursor {
                    variables[i] = Some(c);
                }
                None
            }
        };
        let values: Values = variables.into();
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let poller = self
            .inner
            .add(executor, sql.into(), cursor, values, id, sink, last_hash);
        Ok(SubscriptionHandle {
            registry: Arc::downgrade(&self.inner),
            poller,
            id,
        })
    }

    /// Number of running pollers.
    pub fn poller_count(&self) -> usize {
        self.inner.pollers.lock().expect("lock").len()
    }

    /// Number of cohorts over all pollers.
    pub fn cohort_count(&self) -> usize {
        let pollers: Vec<Arc<Poller>> = self
            .inner
            .pollers
            .lock()
            .expect("lock")
            .values()
            .cloned()
            .collect();
        pollers
            .iter()
            .map(|p| p.state.lock().expect("lock").cohorts.len())
            .sum()
    }
}

impl RegistryInner {
    #[allow(clippy::too_many_arguments)]
    fn add(
        self: &Arc<Self>,
        executor: Arc<dyn SourceExecutor>,
        sql: Arc<str>,
        cursor: Option<usize>,
        values: Values,
        id: u64,
        sink: Arc<dyn Subscriber>,
        last_hash: Option<u128>,
    ) -> Arc<Poller> {
        let key = PollerKey {
            executor: Arc::as_ptr(&executor) as *const () as usize,
            sql,
        };
        // The registry lock is never held while waiting for a poller's state, which a poll
        // holds while delivering results.
        loop {
            let poller = self
                .pollers
                .lock()
                .expect("lock")
                .entry(key.clone())
                .or_insert_with(|| {
                    let poller = Arc::new(Poller {
                        key: key.clone(),
                        executor: executor.clone(),
                        cursor,
                        state: Mutex::new(PollerState::default()),
                        task: Mutex::new(None),
                    });
                    let task = tokio::spawn(run(poller.clone(), self.options));
                    *poller.task.lock().expect("lock") = Some(task.abort_handle());
                    poller
                })
                .clone();
            if poller.join(id, &values, &sink, last_hash) {
                return poller;
            }
            // Stopping: make sure it is unregistered (its `remove` may not have got there
            // yet), then start a new one.
            self.unregister(&poller);
        }
    }

    fn unregister(&self, poller: &Arc<Poller>) {
        let mut pollers = self.pollers.lock().expect("lock");
        if pollers
            .get(&poller.key)
            .is_some_and(|p| Arc::ptr_eq(p, poller))
        {
            pollers.remove(&poller.key);
        }
    }

    fn remove(&self, poller: &Arc<Poller>, id: u64) {
        let mut state = poller.state.lock().expect("lock");
        if let Some(values) = state.members.remove(&id)
            && let Some(cohort) = state.cohorts.get_mut(&values)
        {
            cohort.subscribers.remove(&id);
            if cohort.subscribers.is_empty() {
                state.cohorts.remove(&values);
            }
        }
        if state.members.is_empty() && !state.closed {
            state.closed = true;
            state.cohorts.clear();
            drop(state);
            self.unregister(poller);
            if let Some(task) = poller.task.lock().expect("lock").take() {
                task.abort();
            }
        }
    }
}

async fn run(poller: Arc<Poller>, options: PollerOptions) {
    loop {
        tokio::time::sleep(options.refetch_interval).await;
        poller.poll(options.batch_size).await;
    }
}

impl Poller {
    /// Add a subscriber to the cohort for `values`; false if the poller is stopping.
    fn join(
        &self,
        id: u64,
        values: &Values,
        sink: &Arc<dyn Subscriber>,
        last_hash: Option<u128>,
    ) -> bool {
        let mut state = self.state.lock().expect("lock");
        if state.closed {
            return false;
        }
        let joined = state.epoch;
        let cohort = state
            .cohorts
            .entry(values.clone())
            .or_insert_with(|| Cohort {
                json: values_json(values).into(),
                hash: last_hash,
                mixed: false,
                subscribers: HashMap::new(),
            });
        if cohort.hash != last_hash {
            cohort.mixed = true;
        }
        cohort.subscribers.insert(
            id,
            Member {
                sink: sink.clone(),
                last_hash,
                joined,
            },
        );
        state.members.insert(id, values.clone());
        true
    }

    async fn poll(&self, batch_size: usize) {
        let started = Instant::now();
        let (epoch, cohorts) = {
            let mut state = self.state.lock().expect("lock");
            let epoch = state.epoch;
            state.epoch += 1;
            let cohorts: Vec<(Values, Arc<str>)> = state
                .cohorts
                .iter()
                .map(|(k, c)| (k.clone(), c.json.clone()))
                .collect();
            (epoch, cohorts)
        };
        let batches = cohorts.chunks(batch_size).map(|batch| async move {
            let mut json = String::from("[");
            for (i, (_, values)) in batch.iter().enumerate() {
                if i > 0 {
                    json.push(',');
                }
                json.push_str(values);
            }
            json.push(']');
            let result = self
                .executor
                .execute_multiplexed(&self.key.sql, &json)
                .await;
            (batch, result)
        });
        let results = futures::future::join_all(batches).await;
        let mut state = self.state.lock().expect("lock");
        for (batch, result) in results {
            match result {
                Ok(rows) => {
                    for row in rows {
                        let Some((values, _)) = batch.get(row.index) else {
                            continue;
                        };
                        let (fields, cursor) = split_cursor(row, self.cursor.is_some());
                        match self.cursor {
                            None => state.deliver_live(values, &fields, epoch),
                            Some(i) => state.deliver_stream(values, &fields, cursor, i, epoch),
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(target: "livequery-poller-log", error = %e, "poll failed");
                    for (values, _) in batch {
                        state.deliver_error(values, &e, epoch);
                    }
                }
            }
        }
        tracing::debug!(
            target: "livequery-poller-log",
            cohorts = cohorts.len(),
            total_time = started.elapsed().as_secs_f64(),
            "polled"
        );
    }
}

impl PollerState {
    fn deliver_live(&mut self, values: &Values, fields: &[Option<String>], epoch: u64) {
        let Some(cohort) = self.cohorts.get_mut(values) else {
            return;
        };
        let h = hash(fields);
        if cohort.hash == Some(h) && !cohort.mixed {
            return;
        }
        let mut mixed = false;
        for m in cohort.subscribers.values_mut() {
            if m.last_hash == Some(h) {
                continue;
            }
            if m.joined > epoch {
                mixed = true;
                continue;
            }
            m.sink.on_data(fields, None);
            m.last_hash = Some(h);
        }
        cohort.hash = Some(h);
        cohort.mixed = mixed;
    }

    fn deliver_error(&mut self, values: &Values, error: &Error, epoch: u64) {
        let Some(cohort) = self.cohorts.get_mut(values) else {
            return;
        };
        for m in cohort.subscribers.values_mut() {
            if m.joined <= epoch {
                m.sink.on_error(error);
                m.last_hash = None;
            }
        }
        cohort.hash = None;
        cohort.mixed = true;
    }

    /// Send a non-empty batch and move the subscribers that were part of this poll to the
    /// cohort at the advanced cursor.
    fn deliver_stream(
        &mut self,
        values: &Values,
        fields: &[Option<String>],
        cursor: Option<String>,
        cursor_index: usize,
        epoch: u64,
    ) {
        if is_empty_batch(fields) {
            return;
        }
        let Some(cohort) = self.cohorts.get_mut(values) else {
            return;
        };
        let polled: Vec<u64> = cohort
            .subscribers
            .iter()
            .filter(|(_, m)| m.joined <= epoch)
            .map(|(id, _)| *id)
            .collect();
        for id in &polled {
            cohort.subscribers[id]
                .sink
                .on_data(fields, values[cursor_index].as_deref());
        }
        let Some(cursor) = cursor else {
            return;
        };
        let mut next = values.to_vec();
        next[cursor_index] = Some(cursor);
        let next: Values = next.into();
        // Nobody to move (every subscriber joined after this poll's snapshot): creating the
        // target cohort would leave an empty cohort polled forever.
        if next == *values || polled.is_empty() {
            return;
        }
        let moved: Vec<(u64, Member)> = polled
            .into_iter()
            .filter_map(|id| cohort.subscribers.remove(&id).map(|m| (id, m)))
            .collect();
        if cohort.subscribers.is_empty() {
            self.cohorts.remove(values);
        }
        let target = self.cohorts.entry(next.clone()).or_insert_with(|| Cohort {
            json: values_json(&next).into(),
            hash: None,
            mixed: false,
            subscribers: HashMap::new(),
        });
        for (id, m) in moved {
            target.subscribers.insert(id, m);
            self.members.insert(id, next.clone());
        }
    }
}

/// Split the trailing cursor column off a streaming row.
fn split_cursor(row: MultiplexedRow, streaming: bool) -> (Vec<Option<String>>, Option<String>) {
    let mut fields = row.columns;
    let cursor = if streaming {
        fields.pop().flatten()
    } else {
        None
    };
    (fields, cursor)
}

/// A streaming batch with no rows.
fn is_empty_batch(fields: &[Option<String>]) -> bool {
    fields
        .iter()
        .all(|f| f.as_deref().is_none_or(|t| t == "[]"))
}

fn values_json(values: &[Option<String>]) -> String {
    serde_json::to_string(values).expect("strings serialize")
}

/// A 128-bit hash of a result (the field texts and where they end).
fn hash(fields: &[Option<String>]) -> u128 {
    let mut h = Xxh3::new();
    for f in fields {
        match f {
            None => h.update(&[0]),
            Some(t) => {
                h.update(&[1]);
                h.update(&(t.len() as u64).to_le_bytes());
                h.update(t.as_bytes());
            }
        }
    }
    h.digest128()
}

#[cfg(test)]
mod tests;
