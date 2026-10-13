# Event triggers

`hge-events` implements Hasura V2 event triggers against the same `hdb_catalog` objects V2
uses, so a database can move between V2 and this engine (or be shared by both during a
migration) with events intact.

## What happens at startup and on metadata changes

For every source with at least one event trigger (unless `HASURA_GRAPHQL_DISABLE_EVENTING`):

1. The source catalog (`hdb_catalog.event_log`, `event_invocation_logs`, `insert_event_log()`,
   …) is created from V2's own `init_pg_source.sql`, or migrated to source catalog version 4.
2. Each trigger's `hdb_catalog."notify_hasura_<name>_<OP>"()` function is written when its body
   differs from what is installed, the SQL trigger is created when missing, and functions of
   ops the trigger no longer listens to are dropped.

All of this runs under a Postgres advisory lock, so replicas booting together serialise, and
a restart with unchanged metadata issues no DDL. (V2 only writes trigger DDL on metadata API
calls; doing it on boot also covers a metadata directory applied at startup.)

A trigger whose webhook or headers cannot be resolved from the environment is reported as
inconsistent and gets no SQL trigger, as in V2. Triggers with `request_transform` /
`response_transform` are reported inconsistent until transforms are supported, so events
queue up instead of reaching the webhook with an untransformed body.

## Delivery

Each instance polls every source (`HASURA_GRAPHQL_EVENTS_FETCH_INTERVAL`, default 1000 ms)
and locks up to `HASURA_GRAPHQL_EVENTS_FETCH_BATCH_SIZE` (100) events with
`FOR UPDATE SKIP LOCKED`, so any number of replicas can deliver from the same database
without double delivery. At most `HASURA_GRAPHQL_EVENTS_HTTP_POOL_SIZE` (100) deliveries
run at once. Payloads, headers, retries (`retry_conf`, `Retry-After`), timeouts and
invocation logs follow V2 (see `docs/spec/07-events-and-scheduled-triggers.md`).

On SIGTERM, events fetched but not started are unlocked at once (another replica can take
them immediately); in-flight deliveries get `HASURA_GRAPHQL_GRACEFUL_SHUTDOWN_TIMEOUT`
(60 s) and anything still locked after that is unlocked. Events locked by an instance that
was killed are picked up again after 30 minutes (V2's rule).

## Metadata API

`pg_create_event_trigger`, `pg_delete_event_trigger`, `pg_invoke_event_trigger`,
`pg_redeliver_event`, `pg_get_event_logs`, `pg_get_event_invocation_logs`,
`pg_get_event_by_id` and `pause_event_trigger_cleanups` / `resume_event_trigger_cleanups`
behave like V2 (errors, check order and response shapes are replayed from a V2 recording in
`crates/hge-server/tests/events_api.rs`). `cleanup_event_trigger_logs` answers with V2's
enterprise-only error. Creating or deleting a trigger only edits metadata; resolving the new
metadata installs or drops the SQL triggers and archives a deleted trigger's events.

Differences from V2, on purpose:

* A name is taken when a trigger of that name is in the source's metadata. V2 instead looks
  for the trigger's SQL functions, so SQL left behind by a failed `create_event_trigger`
  blocks the name there; here such leftovers are dropped the next time metadata is resolved.
* A rejected or failed metadata request leaves no SQL trigger changes behind: the stored
  metadata is resolved again, which puts the source back (V2 keeps the trigger it installed
  for a definition it then rejects, e.g. one whose `payload` names a missing column).

`run_sql` and manual invocations record `{"x-hasura-role":"admin"}` and a fresh trace context
as the event's `session_variables` / `trace_context`. Not yet like V2:

* V2 records the caller's whole session (e.g. an `x-hasura-user-id` sent along with the admin
  secret) and continues the caller's trace.
* Numbers in payloads read back through the log APIs, and in manual-invoke payloads, go
  through 64-bit floats, so integers beyond 2^53 and long decimals lose precision.
* The log APIs and `pg_invoke_event_trigger` accept a trigger that is in metadata but
  inconsistent; V2 answers that it does not exist.

## Cron triggers and one-off scheduled events

These live in the metadata database (`hdb_catalog.hdb_cron_events`, `hdb_scheduled_events`
and their invocation logs, part of the V2 metadata catalog). Every instance that processes
events runs:

* a generator (every minute) that keeps 100 upcoming events per cron trigger, using V2's
  cron syntax (5 fields, UTC), and drops future events of removed triggers or of a changed
  schedule when metadata changes;
* a delivery loop (every 10 s) that locks due events with `FOR UPDATE SKIP LOCKED`, marks
  events older than their `tolerance_seconds` as `dead`, and delivers the rest with V2's
  payload, headers, retries and invocation logs. An instance holds a Postgres advisory
  lock for every event it is delivering, so events left `locked` by an instance that died
  are released by the next delivery loop of any instance, and never while their instance
  is alive. (V2 instead unlocks every locked event when it starts. If V2 and this engine
  share a metadata database during a migration, events a V2 instance is delivering look
  orphaned to this engine and can be delivered twice.)

The metadata commands `create_cron_trigger`, `delete_cron_trigger`, `get_cron_triggers`,
`create_scheduled_event`, `delete_scheduled_event`, `get_scheduled_events` and
`get_scheduled_event_invocations` behave like V2 (replayed from a V2 recording in
`crates/hge-server/tests/scheduled_api.rs`). Creating a cron trigger stores its next 100
events at once, replacing it regenerates them and deleting it drops its untried future
events, as in V2. Differences:

* `get_scheduled_events` quotes `order_by` columns, so an unknown column is a plain
  database error; V2 pastes the column into its SQL and reports the whole statement.
* `get_cron_triggers` lists triggers by name (V2's order follows a hash map) and includes
  inconsistent ones.
* Request and response transforms are not supported yet: a cron trigger with one is
  inconsistent, and `create_scheduled_event` rejects them.

On shutdown, an instance stops fetching, gives running deliveries
`HASURA_GRAPHQL_GRACEFUL_SHUTDOWN_TIMEOUT`, and puts unfinished events back to `scheduled`.

The same Cloud Run advice applies: the instances that deliver need CPU between requests.

## Cloud Run

Cloud Run's default "CPU only allocated during requests" throttles background work between
requests, which stalls event delivery. Pick one:

* **Single service:** deploy with `--no-cpu-throttling` (CPU always allocated) and
  `--min-instances=1`, so at least one instance is always polling. Set
  `HASURA_GRAPHQL_GRACEFUL_SHUTDOWN_TIMEOUT` below Cloud Run's 10 s termination grace
  (e.g. `8`) so locks are released before the instance is killed.
* **API + worker (recommended at scale):** run the API service with request-based CPU,
  autoscaling to zero if you like, and `HASURA_GRAPHQL_DISABLE_EVENT_PROCESSING=true` (it
  still keeps SQL triggers up to date). Run a second service from the same image with
  `--no-cpu-throttling --min-instances=1 --max-instances=1` (or more: delivery is
  replica-safe) that does the delivering.

Events are captured by Postgres triggers, so writes made while no instance runs are never
lost; they are delivered when a delivering instance is up again.
