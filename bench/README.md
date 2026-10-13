# Benchmark and parity harness

Runs the official Hasura GraphQL Engine V2 and our Rust engine side by side
on the same Postgres data and the same, unmodified V2 project, then:

- **parity** (`scripts/parity.py`): sends every case in `queries/` to both
  engines and diffs the responses strictly: HTTP status, `Content-Encoding`
  (requests send `Accept-Encoding: gzip`), values, JSON types, the exact text
  of numbers (`15.90` is not `15.9`; `--loose-numbers` relaxes this) and key
  order. Event-trigger delivery counts and payloads are compared too. With `--golden` it compares one
  engine against responses recorded from Hasura in `expected/`, so it works
  as a compatibility test suite without Hasura running.
- **load** (`scripts/run.py`): drives each case with
  [oha](https://github.com/hatoo/oha) and reports RPS, p50/p95/p99 latency,
  peak memory, CPU, idle memory, image size and cold-start time per engine.

Everything is Python 3.9+ standard library plus Docker (Compose v2) and oha.

## Layout

| path | what |
|---|---|
| `project/` | a V2 project exactly as `hasura init` lays it out: `config.yaml`, `metadata/` (v3 format), `migrations/default/` |
| `handler/server.py` | action handler (`trackPriceQuote`) and event-trigger webhook; each engine posts to `/events/<engine>` and the handler keeps counts and recent payloads per engine |
| `docker-compose.yml` | Postgres 16, handler, Hasura `v2.51.0-ce.cli-migrations-v3`, `hasura-serve` (profile `startup`, startup timing only), and `rust-engine` (profile `rust`) |
| `queries/*.graphql` | the case suite, one file per case |
| `expected/` | golden responses recorded from Hasura |
| `results/<label>/` | `results.json` and `summary.md` per load run |

### The sample project

Chinook-style music store, seeded deterministically (no `random()`): 1k
artists, 3k albums, 30k tracks, 2k customers, 20k invoices, 100k invoice
lines. It exercises object/array relationships (FK and manual), a computed
field, a tracked view with custom column names, row-level permissions for a
`user` role driven by `X-Hasura-User-Id` (including relationship filters,
column presets, insert checks, a row limit and aggregations), a synchronous
action with an action relationship, an insert event trigger, a query
collection and a REST endpoint.

Hasura and the Rust engine each get their own database (`hasura`, `rust`)
in the same Postgres server so their writes and event bookkeeping never
interfere.

## Quick start

```sh
cd bench
docker compose up -d --wait                       # postgres + handler + hasura on :8080
python3 scripts/parity.py --a hasura=http://localhost:8080 --golden
cargo install oha                                  # once
python3 scripts/run.py --engine hasura=http://localhost:8080@hasura --label my-run
```

With the Rust engine (built from the repo root `Dockerfile`):

```sh
docker compose --profile rust up -d --build       # adds rust-engine on :8081
python3 scripts/parity.py --a hasura=http://localhost:8080 --b rust=http://localhost:8081
python3 scripts/parity.py --a rust=http://localhost:8081 --golden --report results/parity-rust.md
python3 scripts/run.py \
  --engine hasura=http://localhost:8080@hasura \
  --engine rust=http://localhost:8081@rust-engine \
  --startup-only hasura-serve=http://localhost:8082@hasura-serve \
  --concurrency 32 --concurrency 128 --label 2026-10-20-compare
```

CI (`rust-engine-parity`) runs the golden check with
`--known-failures parity-known-failures.txt`: cases listed there are reported
as known gaps, and the job fails only when an unlisted case fails (a
regression) or a listed case starts passing. A PR that fixes a listed case
deletes its line in the same PR.

If Docker Hub rate-limits pulls, use the mirror images:
`docker compose --env-file .env.mirror ...` and pass
`--compose-arg=--env-file=.env.mirror` to `run.py`.

## What the Rust engine must provide

The `rust-engine` service treats our engine as a drop-in for the
`cli-migrations-v3` image:

- listens on port 8080 inside the container, `GET /healthz` returns 200 once
  it is ready to serve;
- applies `HASURA_GRAPHQL_MIGRATIONS_DIR` (`/hasura-migrations`) and
  `HASURA_GRAPHQL_METADATA_DIR` (`/hasura-metadata`) on boot, like
  cli-migrations does;
- reads the same environment as Hasura: `HASURA_GRAPHQL_ADMIN_SECRET`,
  `HASURA_GRAPHQL_METADATA_DATABASE_URL`, `PG_DATABASE_URL` (referenced via
  `from_env` in `databases.yaml`), `HASURA_GRAPHQL_UNAUTHORIZED_ROLE`,
  `ACTION_BASE_URL` and `EVENT_WEBHOOK_URL` (template/env references in the
  metadata);
- compresses responses the way Hasura does when the client sends
  `Accept-Encoding: gzip` (parity checks `Content-Encoding`).

The `rust-engine` service has no healthcheck yet (the image may not ship
curl); once the engine has a health probe command, add one so
`docker compose --profile rust up --wait` covers it.

Parity's engine names double as event namespaces: use `hasura` and `rust`,
matching `EVENT_WEBHOOK_URL` in the compose file.

## Adding a case

Create `queries/NN_name.graphql`. Header lines configure it:

```graphql
#@ description: what this exercises
#@ role: user                 # x-hasura-role (omit for admin)
#@ user_id: 42                # x-hasura-user-id
#@ variables: {"id": 1}
#@ auth: none                 # omit the admin secret
#@ bench: false               # parity only (error cases)
#@ rest: GET /api/rest/x/1    # call a REST endpoint instead of /v1/graphql
query { ... }
```

Mutations must leave the database as they found it (set constant values,
or insert and delete in the same request) so that load runs never change
what the parity suite sees. Then re-record goldens from a fresh stack:

```sh
docker compose down -v && docker compose up -d --wait
python3 scripts/parity.py --a hasura=http://localhost:8080 --record
```

## Reading the numbers

- Both engines are pinned to the same cores (`ENGINE_CPUSET`, default `0,1`)
  with the same memory limit (`ENGINE_MEM`, default 2g). Pinning instead of a
  CPU quota matters: GHC's `-N` and tokio both size their thread pools from
  the visible CPUs. Postgres, the handler and oha are not pinned and share the
  host, so absolute numbers depend on the machine; compare engines within one
  run, not across runs.
- Engines alternate order from case to case, each with a warmup before
  measurement. oha sends `Accept-Encoding: gzip`, like browsers and most
  clients, so Hasura compresses larger responses.
- After each mutation case the runner waits until that engine's event-trigger
  deliveries stop arriving (`drain` in the summary), so a backlog never eats
  into the next measurement.
- `err` counts non-200 responses and transport errors. GraphQL errors are HTTP
  200, so a `!` marks rows whose response size under load drifted more than
  10% from the validated response; ratios are omitted for such rows.
- `13_action` is bounded by the Python action handler for both engines, and
  `19_mutation_update` makes every request lock the same row, so those two
  measure contention as much as the engine.
- Each case is measured once per run; rerun to gauge variance.
- `startup` is `docker compose start` until `/healthz` is 200, median of N
  restarts against an already-migrated database. For `hasura` it includes
  the cli-migrations wrapper (temporary server, metadata apply, reload);
  `hasura-serve` is the bare `graphql-engine serve` boot.
- Memory is sampled from `docker stats` (about twice a second), so "peak" is
  the highest sample, not a true peak.
- GraphQL errors come back as HTTP 200; `run.py` checks each case returns
  data without `errors` before loading it and skips it otherwise.

## Subscriptions (`scripts/subbench.py`)

Holds thousands of websocket subscribers open on one engine at a time,
then writes to Postgres directly (a long-lived `psql` on the published
port, so the write path is the same for every engine) and measures how
fast each subscriber sees the change.

```sh
python3 scripts/subbench.py \
  --engine hasura=http://localhost:8080@hasura \
  --engine rust=http://localhost:8081@rust-engine \
  --subscribers 1000 --subscribers 5000 --label 2026-10-20-subs
```

- Live queries cycle through four shapes: `user`-role rows (20 distinct
  `X-Hasura-User-Id`s, so 20 cohorts), admin `playlist_by_pk` with 100
  distinct variables, a `user`-role relationship + aggregate, and one query
  every subscriber shares. Every write renames playlists 1-100 with a token,
  which changes every shape's result.
- 50 streaming subscribers (`playlist_stream` from id 900000) receive rows
  inserted during the run.
- Reported per engine and subscriber count: time to connect everyone and
  get every initial result; memory per subscriber (steady subscribed memory
  minus idle memory, divided by subscribers); CPU while subscribed but idle
  (pure polling cost) and during writes; Postgres transactions/s in the
  engine's database (its polling load); commit-to-delivery latency
  p50/p95/p99/max and subscribers that never saw a write; stream delivery
  latency.
- Latency includes each engine's refetch interval
  (`HASURA_GRAPHQL_LIVE_QUERIES_MULTIPLEXED_REFETCH_INTERVAL`, 1 s by
  default for both), so up to 1 s of each figure is the poll interval. Writes
  are spaced `--write-interval` plus a random 0-1 s, so they do not lock
  onto the poll tick and the median samples the whole interval.
- Subscribers connect to the container's address on the compose network,
  not the published port: Docker's userland port proxy falls over at a few
  thousand sockets and adds a hop. `--via-proxy` restores the old path;
  `run.py` does the same for HTTP load (startup timing still uses the
  published port).
- Each run restarts the engine first. Memory per subscriber is measured
  against that freshly started container.
- The client is stdlib asyncio (`scripts/wsclient.py`, both
  `graphql-transport-ws` and Apollo `graphql-ws` via `--protocol`) spread
  over `--workers` processes. Raise `ulimit -n` above the subscriber count.
- Data is restored after each run, so parity goldens keep matching.
