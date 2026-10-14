# Deploying graphql-engine-rust

The engine is one static binary in a small distroless image. It reads the same
`HASURA_GRAPHQL_*` environment variables as Hasura V2, so an existing V2 deployment usually
needs only its image swapped.

| Platform | Guide | Example |
|---|---|---|
| Google Cloud Run | [cloud-run.md](cloud-run.md) | [`deploy/cloud-run/`](../../deploy/cloud-run) |
| Kubernetes | [kubernetes.md](kubernetes.md) | [`deploy/kubernetes/`](../../deploy/kubernetes) |
| Fly.io, Render | [fly-render.md](fly-render.md) | [`deploy/fly/`](../../deploy/fly), [`deploy/render/`](../../deploy/render) |
| Cloudflare | [cloudflare.md](cloudflare.md) | not yet |

## What every platform needs to know

**Port.** The server listens on `HASURA_GRAPHQL_SERVER_PORT`, else `$PORT`, else 8080, on all
interfaces.

**Metadata and migrations.** Either keep metadata in `hdb_catalog` (V2's default, applied with
`hasura metadata apply`), or bake your Hasura CLI project into the image and set
`HASURA_GRAPHQL_METADATA_DIR` and `HASURA_GRAPHQL_MIGRATIONS_DIR`. With those two set the
engine applies migrations and metadata on boot, like V2's `cli-migrations` image.

```dockerfile
# graphql-engine-rust: your build of this repository's Dockerfile
FROM graphql-engine-rust
COPY hasura/migrations /hasura-migrations
COPY hasura/metadata /hasura-metadata
ENV HASURA_GRAPHQL_MIGRATIONS_DIR=/hasura-migrations \
    HASURA_GRAPHQL_METADATA_DIR=/hasura-metadata
```

**Health checks.** Use `GET /healthz`:

| Response | Meaning |
|---|---|
| `200 OK` | Healthy. |
| `200 WARN: inconsistent objects in schema` | Serving, but some metadata is inconsistent. |
| `500 ERROR` | The metadata database does not answer. |
| `500 ERROR: inconsistent objects in schema` | Only with `?strict=true`: inconsistent metadata counts as unhealthy. |

Use `/healthz?strict=true` for a startup check, so a deploy with broken metadata never takes
traffic, and plain `/healthz` for readiness and liveness. Strict readiness would mark every pod
unready at once when metadata becomes inconsistent at runtime (for example after a column is
dropped), taking the whole service down. `/hasura/healthz` is the same endpoint.

**Shutdown.** On `SIGTERM` the engine first closes WebSockets with code 1012 so clients
reconnect elsewhere (taking up to 5 s). Then, in parallel, it stops accepting connections and
gives in-flight HTTP requests up to 30 s, and gives event trigger deliveries, scheduled events
and async actions up to `HASURA_GRAPHQL_GRACEFUL_SHUTDOWN_TIMEOUT` (default 60 s). Events still
locked after that are unlocked so another instance picks them up. The worst case is therefore
5 s + the larger of the two: set the platform's grace period above that, or lower the timeout
to fit the platform.

**Background work.** Besides serving requests, every instance by default polls for event
triggers, scheduled and cron triggers, async actions, subscription results and metadata
changes made by other instances. That work needs CPU between requests. Platforms that
throttle idle instances need the split described below.

| Variable | Effect |
|---|---|
| `HASURA_GRAPHQL_DISABLE_EVENT_PROCESSING=true` | Keep event triggers installed and capturing, but deliver no event triggers, scheduled events or cron triggers from this instance. Does not affect async actions. |
| `HASURA_GRAPHQL_ASYNC_ACTIONS_FETCH_INTERVAL=0` | Process no async actions on this instance (it still accepts async action mutations and answers their queries). |
| `HASURA_GRAPHQL_EVENTS_FETCH_INTERVAL`, `HASURA_GRAPHQL_ASYNC_ACTIONS_FETCH_INTERVAL` | Poll intervals in ms. |
| `HASURA_GRAPHQL_SCHEMA_SYNC_POLL_INTERVAL` | How often instances check for metadata changed elsewhere (ms; 0 disables). |

A common shape: N autoscaled API replicas with `HASURA_GRAPHQL_DISABLE_EVENT_PROCESSING=true`
and `HASURA_GRAPHQL_ASYNC_ACTIONS_FETCH_INTERVAL=0`, plus one always-on worker replica with the
default settings. Both variables are needed: the first leaves async actions running.

**Database connections.** Each instance opens up to `pool_settings.max_connections` per source
(default 50, `HASURA_GRAPHQL_PG_CONNECTIONS` for sources without pool settings); event trigger
deliveries use these source pools. On the metadata database each instance opens up to 4
connections for metadata, `max(HASURA_GRAPHQL_ASYNC_ACTIONS_FETCH_BATCH_SIZE, 5) + 4` (14 by
default) for the async action log, and, where event processing is enabled, 5 more for
scheduled and cron triggers. Keep `instances × connections` below the database's limit, or put
PgBouncer (transaction mode, with `HASURA_GRAPHQL_USE_PREPARED_STATEMENTS=false`) in front.

**Logs.** One JSON object per line on stdout, in V2's format:
`{"timestamp":"…","level":"info","type":"http-log","detail":{…}}`. Choose types with
`HASURA_GRAPHQL_ENABLED_LOG_TYPES` (default `startup,http-log,webhook-log,websocket-log,jwk-refresh-log`)
and the minimum level with `HASURA_GRAPHQL_LOG_LEVEL`. Log pipelines built for V2 keep
working. To cut log volume on busy APIs, drop `http-log` from the list.
