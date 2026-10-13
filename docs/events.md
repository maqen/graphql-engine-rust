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
