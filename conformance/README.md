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
                   snapshot: <area>/<suite> takes the next three files from that suite
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

### JWTs

A header value (or a value inside a websocket message) can be a token the runner signs when
the case runs, so recorded cases never expire:

```yaml
headers:
  Authorization:
    prefix: 'Bearer '
    jwt:
      key: rsa                     # rsa (RS512), es (ES256), ed25519 (EdDSA), or {hs256: <secret>}
      claims:
        exp: '{{now+3600}}'        # {{now}}, {{now+N}}, {{now-N}}: Unix times
        https://hasura.io/jwt/claims: {x-hasura-default-role: user, x-hasura-allowed-roles: [user]}
```

The keys are in `conformance/keys/`. In `suite.yaml` env, `{{public_key:rsa}}` is replaced by
that public key, escaped for a JSON string, for `HASURA_GRAPHQL_JWT_SECRET`. The JWT suites
whose claims depend on the current time are written by `conformance/tools/hand/jwt_cases.py`
and then recorded; the rest use pre-signed tokens from `conformance/tools/gen_jwt_suites.py`.

### Websocket steps

Steps can also drive GraphQL over websockets on `/v1/graphql`, in either protocol. A step is
a mapping with one `ws_*` key; connections are named by `conn` (default `default`) and closed
when the case ends:

```yaml
- ws_connect: {protocol: graphql-ws}            # or graphql-transport-ws; url, headers optional
- ws_send:
    message: {type: connection_init, payload: {headers: {X-Hasura-Admin-Secret: '{{HASURA_GRAPHQL_ADMIN_SECRET}}'}}}
- ws_expect: {message: {type: connection_ack}}
- ws_send:
    message: {id: '1', type: start, payload: {query: 'subscription { author { id } }'}}
- ws_expect:                                    # the next message with the same id
    message: {id: '1', type: data, payload: {data: {author: [{id: 1}]}}}
    timeout_ms: 15000                           # optional
- ws_expect_silence: {ms: 2000, id: '1'}        # nothing (with this id, if given) arrives
- ws_expect_close: {code: 4400, reason: ...}    # the server closes; code and reason optional
```

`{{HASURA_GRAPHQL_ADMIN_SECRET}}` in a sent message or a connect header is replaced by the
suite's admin secret (the runner does not add it to websockets by itself). Keep-alives (`ka`,
`pong`) are ignored and pings answered. A message's top-level keys compare in any order and
its `payload` like an HTTP response; a top-level value of `'{{any string}}'` matches any
string, for error texts from V2's JSON parser. An HTTP step between websocket steps (for
example `run_sql` on `/v2/query` to change data under a live query) runs in order. With
`--record`, a failed `ws_expect` or `ws_expect_close` is rewritten with what arrived.

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

Tests that are custom Python (the websocket protocol and subscription tests in
`test_subscriptions.py`) are translated into case files by hand, marked
`upstream: {..., by_hand: true}`, and recorded from Hasura after checking the result against
upstream's assertions. The tool keeps them when it regenerates a suite (list the class in
`HAND_PORTED` so its setup is snapshotted); `subscriptions/protocol*` hold the classes that
need no database. Use `--only <regex>` to regenerate some classes; their entries in
`upstream-coverage.yaml` are updated and the others kept.

The fixtures come from [hasura/graphql-engine](https://github.com/hasura/graphql-engine)
(Apache-2.0), `server/tests-py`, at tag v2.51.0.
