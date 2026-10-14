# Kubernetes

Example manifests: [`deploy/kubernetes/`](../../deploy/kubernetes) (Deployment, Service,
PodDisruptionBudget, HorizontalPodAutoscaler and an optional event worker). Apply with
`kubectl apply -k deploy/kubernetes` after setting the image and the `hasura` Secret.

## Probes

* **Startup:** `GET /healthz?strict=true`, so a new pod with inconsistent metadata never
  takes traffic and a broken rollout stalls instead of replacing healthy pods. Give it a
  generous `failureThreshold` when `HASURA_GRAPHQL_MIGRATIONS_DIR` runs long migrations on boot.
* **Readiness:** plain `GET /healthz`, so pods without a metadata database get no traffic. Do
  not use `strict=true` here: metadata that becomes inconsistent at runtime (a dropped column,
  say) is inconsistent on every pod at once, so strict readiness would take the whole service
  out of its endpoints.
* **Liveness:** `GET /healthz` with a generous `failureThreshold`, so a short database outage
  does not restart every pod at once.

Probes must use `GET`, as with V2.

## Shutdown and rollouts

On `SIGTERM` the engine closes WebSockets with 1012 (up to 5 s), then in parallel stops
accepting connections, waits up to 30 s for HTTP requests and up to
`HASURA_GRAPHQL_GRACEFUL_SHUTDOWN_TIMEOUT` (default 60 s) for event deliveries and async
actions. Set `terminationGracePeriodSeconds` above the `preStop` sleep + 5 s + the larger of
the two (the example uses 75 = 5 + 5 + 60, plus margin). A short `preStop` sleep (the Kubernetes 1.30+ `sleep` action, since the image has no shell) lets the endpoint removal reach every node before the pod
stops accepting connections.

## Scaling

All replicas can run every background worker: events and async actions are claimed with row
locks, so no event is delivered twice. For large deployments, run API replicas with
`HASURA_GRAPHQL_DISABLE_EVENT_PROCESSING=true` (no event, scheduled or cron trigger delivery)
and `HASURA_GRAPHQL_ASYNC_ACTIONS_FETCH_INTERVAL=0` (no async action processing) behind an HPA,
and a small fixed worker Deployment ([`worker.yaml`](../../deploy/kubernetes/worker.yaml)) with
the defaults.

Metadata changes made through one replica reach the others through
`hdb_catalog.hdb_schema_notifications`, polled every `HASURA_GRAPHQL_SCHEMA_SYNC_POLL_INTERVAL`
ms.
