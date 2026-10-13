# Fly.io and Render

## Fly.io

[`deploy/fly/fly.toml`](../../deploy/fly/fly.toml):

```sh
cp deploy/fly/fly.toml .            # next to the Dockerfile
fly launch --no-deploy --copy-config
fly secrets set HASURA_GRAPHQL_DATABASE_URL=postgres://… HASURA_GRAPHQL_ADMIN_SECRET=…
fly deploy
```

* `auto_stop_machines` stops idle machines, which stops event trigger and scheduled trigger
  delivery and async actions with them. The example keeps one machine running
  (`min_machines_running = 1`); set `auto_stop_machines = "off"` when all of them must poll.
* `kill_timeout = 40` covers the worst-case shutdown: up to 5 s closing WebSockets, then the
  larger of the 30 s HTTP drain and `HASURA_GRAPHQL_GRACEFUL_SHUTDOWN_TIMEOUT` (25 in the
  example).
* WebSockets work through Fly's proxy with no extra settings.

## Render

[`deploy/render/render.yaml`](../../deploy/render/render.yaml) is a Blueprint with a web
service built from the repository's Dockerfile and `healthCheckPath: /healthz`. Render keeps
paid web services running, so background workers run normally. Free instances sleep when idle,
which pauses event delivery like a stopped Fly machine. Render gives instances 30 s after
`SIGTERM` by default, less than the engine's worst case (5 s closing WebSockets + the larger of
the 30 s HTTP drain and `HASURA_GRAPHQL_GRACEFUL_SHUTDOWN_TIMEOUT`). The example raises it with
`maxShutdownDelaySeconds: 40` (the service's shutdown delay setting in the dashboard) and sets
`HASURA_GRAPHQL_GRACEFUL_SHUTDOWN_TIMEOUT=25`.
