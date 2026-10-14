# Cloudflare

**Status: the engine does not run on Cloudflare Workers yet.** A Workers build is a planned,
separate workstream (compiling the request path to WebAssembly). This page says what works
today and what that port has to solve.

## What works today: Cloudflare Containers

Cloudflare Containers run ordinary Linux images on Cloudflare's network, started and routed by
a Worker. The standard image runs there unchanged, with the same caveats as scale-to-zero
platforms: containers that sleep when idle stop delivering event triggers, scheduled events
and async actions. Keep at least one instance awake (or run those workers elsewhere and set
`HASURA_GRAPHQL_DISABLE_EVENT_PROCESSING=true` and `HASURA_GRAPHQL_ASYNC_ACTIONS_FETCH_INTERVAL=0`
on the containers). When instances are far from the database, put a regional connection pooler
in front of Postgres (or route through a Worker using Hyperdrive): Hyperdrive is a Workers
binding, so a container cannot connect to it directly.

Alternatively, run the engine on any other platform in this guide and put Cloudflare in front
as a CDN or proxy. WebSockets pass through Cloudflare's proxy.

## What a Workers port needs

* **Database access.** Workers cannot use the native Postgres driver. The port needs a driver
  over Workers TCP sockets (`connect()`) or Hyperdrive, without prepared-statement caching
  across requests.
* **No long-running pollers.** Event triggers, scheduled triggers, async actions, live query
  polling and schema sync all poll in the background. On Workers they would move to Cron
  Triggers, Queues and Durable Objects (one per subscription cohort, holding the WebSockets).
* **State per isolate.** The schema cache is built from metadata on cold start; it would be
  stored precomputed (for example in KV) to keep cold starts short.
* **CPU limits.** Large introspection responses and big result sets must fit the per-request
  CPU budget.

The core crates keep I/O behind traits partly so that this port stays possible.
