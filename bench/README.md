# Benchmark and parity harness

Runs the official Hasura GraphQL Engine V2 and our Rust engine side by side
on the same Postgres data and the same, unmodified V2 project, then:

- **parity** (`scripts/parity.py`): sends every case in `queries/` to both
  engines and diffs the JSON responses strictly (status, values, JSON types,
  key order) plus event-trigger deliveries. With `--golden` it compares one
  engine against responses recorded from Hasura in `expected/`, so it works
  as a compatibility test suite without Hasura running.
- **load** (`scripts/run.py`): drives each case with
  [oha](https://github.com/hatoo/oha) and reports RPS, p50/p95/p99 latency,
  peak memory, CPU, idle memory, image size and cold-start time per engine.

Everything is Python 3 standard library plus Docker and oha.

## Layout

| path | what |
|---|---|
| `project/` | a V2 project exactly as `hasura init` lays it out: `config.yaml`, `metadata/` (v3 format), `migrations/default/` |
| `handler/server.py` | action handler (`trackPriceQuote`) and event-trigger webhook that counts deliveries |
| `docker-compose.yml` | Postgres 16, handler, Hasura `v2.51.0.cli-migrations-v3`, `hasura-serve` (startup-only), and `rust-engine` (profile `rust`) |
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
  metadata).

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

- Both engines run with the same limits (`ENGINE_CPUS`, default 2;
  `ENGINE_MEM`, default 2g). Postgres and oha are unconstrained and share the
  host, so absolute numbers depend on the machine; compare engines within one
  run, not across runs.
- Cases are interleaved per engine, each with a warmup before measurement.
- `startup` is `docker compose start` until `/healthz` is 200, median of N
  restarts against an already-migrated database. For `hasura` it includes
  the cli-migrations wrapper (temporary server, metadata apply, reload);
  `hasura-serve` is the bare `graphql-engine serve` boot.
- Memory is sampled from `docker stats` about once a second, so "peak" is the
  highest sample, not a true peak.
- GraphQL errors come back as HTTP 200; `run.py` checks each case returns
  data without `errors` before loading it and skips it otherwise.
