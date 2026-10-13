# Conformance suite

Hasura V2's own integration tests, ported to a data-driven format and run against our engine
in CI. Every expected response was recorded from the real `hasura/graphql-engine:v2.51.0-ce`,
and CI re-checks them against that image whenever cases or the runner change, so a case here
is a statement about what V2 actually does.

| path | what |
|---|---|
| `suites/<area>/<suite>/` | one suite: a database, metadata and the cases that run on it |
| `expected-failures.txt` | case ids our engine does not pass yet |
| `upstream-coverage.yaml` | every upstream tests-py class and test, and whether it was ported (or why not) |
| `tools/port_tests_py.py` | regenerates `suites/` and `upstream-coverage.yaml` from upstream |
| `../crates/hge-conformance` | the runner |

The current pass rate per area is published to the project's
`hasura-rs/conformance/status.md` and appears in the summary of every CI run.

## Running it

Needs a Postgres superuser URL. Use PostGIS 15 (some suites need PostGIS; the dumps were
taken from 15):

```sh
docker run -d --name conf-pg --network host -e POSTGRES_PASSWORD=postgres -e PGPORT=55432 postgis/postgis:15-3.4
export CONFORMANCE_PG_URL=postgres://postgres:postgres@localhost:55432/postgres

cargo build -p graphql-engine -p hge-conformance
target/debug/hge-conformance rust                       # our engine
target/debug/hge-conformance --filter queries/ rust     # a subset (substring of the case id)
target/debug/hge-conformance hasura                     # Hasura itself (needs docker)
```

`rust` starts `target/debug/graphql-engine` once per suite with the suite's environment.
`hasura` runs the image with `--network host`, so Postgres must be reachable at the same URL
from a container. Both print a per-area table and every unexpected failure with the first
differing JSON path; `--report-md` / `--report-json` write them to files.

The `rust` run fails when

* a case fails that is not in `expected-failures.txt` (a regression), or
* a case in `expected-failures.txt` passes: delete its line in the same PR (or run with
  `--write-expected-failures` to rewrite the list from a full run).

## Format

```text
suites/<area>/<suite>/
  suite.yaml       admin_secret: bool, env: {HASURA_GRAPHQL_*: ...}, isolation: none|data|database
  setup.sql        schema and data, applied to a fresh database
  data.sql         isolation: data only: the rows restored before every case
  metadata.json    V2 metadata (export_metadata format); sources on
                   {"from_env": "HASURA_GRAPHQL_DATABASE_URL"} point at the suite's database
  cases/<case>.yaml
```

A case id is `<area>/<suite>/<case>`. A case file is a list of steps in tests-py's own format:

```yaml
upstream: {file: test_graphql_queries.py, class: TestGraphQLQueryBasicCommon, method: test_select_query_author}
steps:
- description: Select author
  url: /v1/graphql
  status: 200
  headers: {X-Hasura-Role: user, X-Hasura-User-Id: '1'}
  query:
    query: '{ author { id name } }'
  response:
    data:
      author: [{id: 1, name: Author 1}]
  resp_headers: {content-type: application/json; charset=utf-8}   # optional
```

With `admin_secret: true` the engine runs with an admin secret that is added to every request
(unless the case sets `admin_secret_header: false`). `isolation` says what is reset between
cases: nothing (`none`, read-only suites), the rows of every table (`data`, mutation suites),
or the whole database and metadata (`database`, suites whose cases change schema or
metadata).

Responses compare exactly: status, body and the listed response headers. Object key order
matters inside GraphQL `data` (it follows the selection set) and nowhere else; floats may
differ in the last bit (Postgres float aggregates do, on V2 too).

## Adding cases

Feature threads add cases for what they build, in the same format, under the area they own:

1. Create `suites/<area>/<suite>/` with `suite.yaml`, `setup.sql`, `metadata.json` (an
   `export_metadata` of a V2 project, sources on `from_env: HASURA_GRAPHQL_DATABASE_URL`).
2. Write the case files with requests only (leave `response` out).
3. Record the responses from Hasura: `hge-conformance --filter <area>/<suite>/ hasura --record`.
   Read what was recorded: it is V2's behaviour, quirks included.
4. Run `hge-conformance --filter <area>/<suite>/ rust` and list the cases we do not pass yet in
   `expected-failures.txt`.

## Regenerating the upstream port

```sh
git clone --depth 1 --branch v2.51.0 https://github.com/hasura/graphql-engine /tmp/hge
python3 conformance/tools/port_tests_py.py --upstream /tmp/hge/server/tests-py --pg-url "$CONFORMANCE_PG_URL"
```

For each upstream test class that only needs Postgres (no webhook, JWT, remote schema or
action servers) and whose tests only call `check_query_f`, the tool runs the upstream setup
on Hasura, snapshots it (`pg_dump`, `export_metadata`), re-applies the snapshot the way the
runner does, and records every request's response. Upstream's YAML is used to confirm each
response (same status, and the same body or both errors, since tests-py does not compare
error messages); cases that disagree with it, or that answer differently on a second run,
are left out. HTTP transport only: tests-py's websocket replay of the same fixtures is not
ported. Classes and tests left out are listed in `upstream-coverage.yaml` with the reason;
most need helper servers (actions, events, remote schemas, auth webhooks, JWT) that a later
version of the runner can provide.

The fixtures come from [hasura/graphql-engine](https://github.com/hasura/graphql-engine)
(Apache-2.0), `server/tests-py`, at tag v2.51.0.
