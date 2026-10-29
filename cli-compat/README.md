# Hasura CLI compatibility

The official `hasura` CLI (v2.51.0) driven through a whole project lifecycle against Hasura
v2.51.0-ce and against our engine, with every step diffed. This is the roadmap's "drop-in"
test: an existing V2 project's CLI workflow must behave the same on both.

The project is `bench/project` (copied, never modified). Each engine gets a fresh database and
its own copy of the project. Two scenarios run:

* `cli/…`: start the engine empty, then `metadata apply`, `migrate apply --all-databases`,
  `metadata reload / export / diff / clear / inconsistency …`, `migrate create / status /
  apply (--down, --goto, --version --type down, --skip-execution) / delete --server / squash`,
  `migrate create --from-server`, `seed create --from-table / apply`, `deploy`.
* `boot/…`: the `cli-migrations-v3` image flow. V2 runs that image with the project's
  `metadata/` and `migrations/` mounted; ours reads `HASURA_GRAPHQL_METADATA_DIR` /
  `HASURA_GRAPHQL_MIGRATIONS_DIR`. Then the CLI checks `migrate status`, inconsistencies,
  `metadata export` and `metadata diff`.

* `console/…`: the server-hosted console (`HASURA_GRAPHQL_ENABLE_CONSOLE`, with V2's console
  build as `HASURA_GRAPHQL_CONSOLE_ASSETS_DIR`) in headless Chromium, with the project
  deployed. It logs in with the admin secret and opens the API explorer, the Data pages of a
  few tables and a view, the SQL page, actions, remote schemas, events, REST, allow list and
  the metadata settings. Each page is compared on every API request it made (set-wise) with
  the engine's response, and on its uncaught JavaScript errors. Requests to other hosts
  (fonts, release checks) are blocked. `run_sql` rows are compared without the order
  Postgres aggregated them in, and object ids and the database name are normalised. Needs
  `pip install playwright` and `playwright install chromium` (or `--chromium <path>`).

A step matches when the CLI's exit code and output, every HTTP request it made and the
engine's response (status and JSON body), and the project files it left behind are the same
on both engines. The CLI reaches the engine through a recording proxy in `run.py`.

Normalised before comparing: timestamps, UUIDs, the proxy's address, migration versions the
CLI derives from the clock, the `pg_dump` version header and `SET` lines (V2 ships pg_dump 18),
and the order of inconsistent objects (V2 lists them in hash map order).

## Running it

Needs Docker, `psql`, `pg_dump`, a Postgres superuser URL reachable from containers with
`--network host`, and the CLI binary (the GitHub release of v2.51.0 has no CLI asset; take it
from the image):

```sh
id=$(docker create mirror.gcr.io/hasura/graphql-engine:v2.51.0-ce.cli-migrations-v3)
docker cp "$id:/bin/hasura-cli" /tmp/hasura && docker rm "$id"

cargo build -p graphql-engine
cli-compat/run.py --hasura-cli /tmp/hasura --pg postgres://postgres:postgres@localhost:5432/postgres
cli-compat/run.py --hasura-cli /tmp/hasura --hasura-results cli-compat-results/hasura.json  # reuse V2's run
cli-compat/run.py --hasura-cli /tmp/hasura --scenario boot --filter migrate
cli-compat/run.py --hasura-cli /tmp/hasura --scenario console --screenshots  # PNGs per page
```

Results land in `cli-compat-results/` (`hasura.json`, `rust.json`, `report.md` with a diff per
differing step). `NODE_OPTIONS` is removed from the CLI's environment: it breaks the CLI's
bundled `cli-ext` (which turns `actions.graphql` into metadata) with "exit status 4".

## expected-failures.txt

Step ids that still differ. CI fails on a differing step that is not listed (a regression) and
on a listed step that now matches: delete its line in the same PR, or rewrite the list from a
full run with `--write-expected-failures`.

## Our CLI (`--rust-cli`)

`crates/hge-cli` builds our own `hasura` binary. `--rust-cli target/debug/hasura` runs the
`cli/…` and `boot/…` scenarios twice against our engine, once with the official CLI and once
with ours, plus the `rust-cli/…` steps (flag and argument errors, aliases, `init`, `metadata
diff` of local edits and its `--type` variants, `seed apply --all-databases`, `migrate delete
--all`). A step matches when the exit code, the output, the project files and the server state
afterwards (metadata, inconsistencies and the CLI's catalog state) are the same. The HTTP
traffic is not compared: our CLI may call the server differently.

The official CLI's first-run notice about usage statistics is dropped from both outputs, since
ours collects none. Known differences are listed in `expected-failures-rust-cli.txt`.

```sh
cargo build -p graphql-engine -p hge-cli
cli-compat/run.py --hasura-cli /tmp/hasura --rust-cli target/debug/hasura
```
