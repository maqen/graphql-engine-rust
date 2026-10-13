# Google Cloud Run

Cloud Run is a good fit for the API: the image starts in well under a second and reads
`$PORT`. The decisions are about background work and WebSockets.

## Quick start

```sh
gcloud run deploy hasura \
  --image=REGION-docker.pkg.dev/PROJECT/REPO/graphql-engine-rust:TAG \
  --region=REGION \
  --set-secrets=HASURA_GRAPHQL_DATABASE_URL=hasura-db-url:latest,HASURA_GRAPHQL_ADMIN_SECRET=hasura-admin-secret:latest \
  --add-cloudsql-instances=PROJECT:REGION:INSTANCE \
  --cpu=1 --memory=512Mi \
  --concurrency=250 \
  --timeout=3600 \
  --min-instances=1 \
  --no-cpu-throttling \
  --set-env-vars=HASURA_GRAPHQL_GRACEFUL_SHUTDOWN_TIMEOUT=4
```

The same as a declarative service: [`deploy/cloud-run/service.yaml`](../../deploy/cloud-run/service.yaml)
(`gcloud run services replace service.yaml`).

## CPU always allocated or throttled?

With the default "CPU only during requests", Cloud Run throttles an instance almost to zero
between requests. An open WebSocket is an active request, so an instance with subscribed clients
keeps its CPU and live queries and streaming subscriptions keep refreshing. The engine keeps
serving queries, mutations and actions correctly, but its other pollers stall while no request
(and no WebSocket) is open:

* event triggers, cron triggers and scheduled events are delivered late, or not at all on an
  idle service,
* async actions are not picked up,
* metadata changed through another instance is noticed late.

Choose one of these:

1. **One service, CPU always allocated** (`--no-cpu-throttling`, `--min-instances=1`). The
   simplest option; you pay for idle instances.
2. **Split API and worker** (recommended when traffic is spiky):
   * `hasura-api`: CPU throttled, autoscaled, `--min-instances=0` or more,
     `HASURA_GRAPHQL_DISABLE_EVENT_PROCESSING=true` (stops event, scheduled and cron trigger
     delivery) and `HASURA_GRAPHQL_ASYNC_ACTIONS_FETCH_INTERVAL=0` (stops async action
     processing). Subscriptions work here too: an open WebSocket is an active request.
   * `hasura-worker`: `--no-cpu-throttling --min-instances=1 --max-instances=1`, default
     settings, `--ingress=internal`. It delivers events, scheduled events and async actions.
   Both point at the same databases and metadata.
3. **Throttled only**, when you use no event triggers, scheduled triggers or async actions.
   Subscriptions still work, since an open WebSocket keeps the instance's CPU.

## WebSockets and subscriptions

Cloud Run supports WebSockets, but every connection is a request bound by `--timeout`
(maximum 3600 s). After that Cloud Run closes it and clients reconnect, which Apollo and
`graphql-ws` clients do automatically. On deploys and scale-in the engine closes sockets with
code 1012 so clients reconnect to a new instance at once. Use `--session-affinity` only if
your clients cannot resubscribe cheaply.

Each open socket counts against `--concurrency`; raise it (250 to 1000) for subscription-heavy
services.

## Shutdown

Cloud Run sends `SIGTERM` and kills the instance 10 s later. The engine spends up to 5 s
closing WebSockets before the worker shutdown starts, so set
`HASURA_GRAPHQL_GRACEFUL_SHUTDOWN_TIMEOUT=4`: in-flight event deliveries and async actions then
finish or are unlocked for another instance within 9 s. HTTP requests still running at 10 s are
cut off by the kill.

## Health checks

```yaml
startupProbe:
  httpGet: { path: /healthz, port: 8080 }
  periodSeconds: 2
  failureThreshold: 30
livenessProbe:
  httpGet: { path: /healthz, port: 8080 }
  periodSeconds: 15
  failureThreshold: 4
```

Plain `/healthz` fails only when the metadata database is unreachable. To keep a revision with
inconsistent metadata from taking traffic, use `/healthz?strict=true` for the startup probe.

## Cloud SQL

With `--add-cloudsql-instances`, connect through the Unix socket Cloud Run mounts:

```
postgresql://USER:PASSWORD@/DB?host=/cloudsql/PROJECT:REGION:INSTANCE
```

Size pools so `max instances × max_connections` stays under the instance's connection limit
(`--max-instances` caps it).

## Logs

Cloud Logging parses each JSON line into `jsonPayload`. V2's format names the level `level`,
not `severity`, so filter with `jsonPayload.level="error"` and
`jsonPayload.type="http-log"`.
