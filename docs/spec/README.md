# Hasura GraphQL Engine V2 — compatibility spec (index)

This folder specifies what a drop-in Rust replacement for Hasura GraphQL Engine V2 must match so that an
existing V2 project (metadata directory, migrations, permissions, actions, event triggers, remote schemas)
runs against it unchanged. Postgres is the only backend in scope; behaviour targeted is the open-source
(CE) edition. Every doc was written from the Haskell source and then checked by an independent reviewer
against that source. Anything not verified is marked "(inferred)" inside the docs.

## Compatibility target

- **Upstream reference:** `hasura/graphql-engine` master at commit `895e7e9` (2026-10-12), which is the
  source these docs were read from. The latest released V2 is **v2.51.0**; for side-by-side testing use the
  Docker image `hasura/graphql-engine:v2.51.0-ce` (the plain tag is the EE build). See 14.
- **Catalog version:** 48 (metadata DB, `hdb_catalog`), source catalog version 4 (per-source event tables).
- **Known drift between master and v2.51.0:** catalog version is 48 in both. v2.51.0 has every serve flag
  in master except `--remote-schema-healing-interval` (doc 11 lists which release added each recent flag).
  v2.51.0 still imports Postgres `COMMENT`s as GraphQL descriptions; master does not (doc 02).
  No v2.51.1 or v2.52.0 tag exists. Golden tests should be pinned
  to one upstream image version.
- **Byte-level details that matter:** aeson sorts object keys alphabetically in most JSON Hasura emits
  (metadata export nested objects, webhook payloads, `/v1/version`), except where a hand-written ordered
  serializer is used. Each doc calls out which applies.

## Documents

| # | Doc | Covers |
|---|-----|--------|
| 01 | [Metadata format](01-metadata-format.md) | CLI metadata directory (config v3/v2), full metadata JSON model as serde-style types, catalog DDL and storage, every `/v1/metadata` op, consistency/inconsistent objects, migrations and what the CLI calls |
| 02 | [GraphQL schema generation](02-graphql-schema-generation.md) | Root fields, arguments, naming conventions and customization, every generated type, comparison operators, Postgres→GraphQL scalar mapping, enum tables, functions, computed fields, introspection ordering, Relay |
| 03 | [Query and mutation semantics](03-query-mutation-semantics.md) | SQL shapes Hasura emits (json_agg, lateral joins, EXISTS filters), parameterisation and session variables, mutations and permission checks, transactions, Postgres error mapping |
| 04 | [Permissions](04-permissions.md) | Insert/select/update/delete rules, bool exps and session variables, presets, backend_only, inherited roles, function/action/remote-schema permissions, validate_input |
| 05 | [Authentication](05-authentication.md) | Admin secret, JWT (all config fields, JWK refresh), auth webhook, unauthorized role, CORS, WebSocket auth |
| 06 | [Actions](06-actions.md) | Action metadata and custom types, schema, handler payloads/headers/errors, async actions and `hdb_action_log`, request/response transforms, Kriti template language |
| 07 | [Event and scheduled triggers](07-events-and-scheduled-triggers.md) | Trigger DDL and functions, event_log tables, delivery loop, payloads, retries, cron and one-off scheduled events, Cloud Run considerations |
| 08 | [Remote schemas and relationships](08-remote-schemas-and-relationships.md) | Remote schema metadata, introspection, customization, forwarding, remote schema permissions, remote relationships and the join execution model |
| 09 | [Subscriptions](09-subscriptions.md) | WebSocket protocols (graphql-ws and graphql-transport-ws), lifecycle and close codes, multiplexed live queries, streaming subscriptions |
| 10 | [HTTP APIs](10-http-apis.md) | Every route, request/response bodies, `/v2/query` and `/v1/query` (run_sql, DML, bulk), version/healthz/config, CORS, compression |
| 11 | [Config and env vars](11-config-and-env-vars.md) | Every flag and env var with defaults and priority, log line formats (http-log etc.), startup and shutdown |
| 12 | [Errors](12-errors.md) | Error envelopes, status code rules, the `code` catalogue, exact messages for common errors |
| 13 | [Misc features](13-misc-features.md) | Allow lists, query collections, RESTified endpoints and OpenAPI, API limits, introspection control, query tags, OpenTelemetry, which features CE only stores |
| 14 | [Upstream test suites](14-upstream-test-suites.md) | How to run upstream pytest/api-tests against our engine, differential testing against `v2.51.0-ce`, benchmarks |

## Priority ordering (by how commonly real projects depend on it)

Tier 1 — every V2 project needs these; nothing works without them.
1. **Metadata load and storage** (01): read `hdb_catalog.hdb_metadata`, `replace_metadata`/`export_metadata`/`reload_metadata`, track tables, relationships, catalog bootstrap. Also loading a metadata directory at boot.
2. **Schema generation for tables** (02): select / by_pk / aggregate, bool_exp, order_by, insert/update/delete, relationships, naming defaults.
3. **Query and mutation execution** (03), including response JSON built in Postgres and the error mapping.
4. **Permissions and session variables** (04): row filters, column sets, presets, insert/update checks, per-role schemas.
5. **Authentication** (05): admin secret, JWT (claims namespace, claims_map, JWK URL), unauthorized role.
6. **HTTP surface and errors** (10, 12): `/v1/graphql`, `/v1/metadata`, `/v2/query` `run_sql` (needed by `hasura migrate apply`), `/healthz`, `/v1/version`, error envelopes.
7. **Config/env vars** (11): the `must` rows only.

Tier 2 — used by most production projects.
8. **Event triggers** (07) — very common; at-least-once delivery semantics and the exact payload.
9. **Actions, sync** (06) — custom business logic via webhooks; payload/header/error formats.
10. **Subscriptions, live queries** (09) over both WebSocket protocols.
11. **Tracked functions and computed fields** (02, 03).
12. **Cron triggers and scheduled events** (07).
13. **Auth webhook mode** (05).
14. **Legacy `/v1/query`** (10) — required to run the upstream pytest suite (its setup uses it), and some older tooling.

Tier 3 — used by a meaningful minority.
15. Remote schemas and remote relationships (08).
16. Async actions, action relationships, request/response transforms and Kriti (06).
17. Streaming subscriptions (09).
18. Inherited roles (04), enum tables (02), allow lists and RESTified endpoints (13).
19. Naming convention `graphql-default` and source customization (02).

Tier 4 — rare, or stored-only in CE.
20. Relay endpoint (02, 10), Apollo Federation (02, 13), native queries / logical models (01, 02), validate_input (04).
21. Features CE accepts in metadata but does not enforce (API limits, per-role introspection disabling, metrics config, OpenTelemetry export, `@cached`): accept and round-trip the metadata, no behaviour needed (13).
22. Non-Postgres backends and EE-only features: out of scope.

## Decisions the implementation needs to make

The reviewers found upstream behaviour that is surprising or buggy. Each needs a "copy" or "fix" decision;
the default suggestion is in brackets. Details are in the referenced section.

- **Key order in JSON outputs** (01, 07, 10): match aeson's sorted keys where upstream sorts. [copy]
- **Error message text parity** (12, 14): pytest only warns on message mismatches; the Haskell api-tests compare exactly. Messages produced by aeson's parser can't be matched byte-for-byte. [match codes, paths and Hasura-authored messages exactly; best effort for aeson text]
- **Mutation `returning` ignores the select row filter** (03, 04): a role can get back rows it cannot select. [copy — clients may depend on it; document it]
- **JWK refresh only in admin-secret + JWT mode** (05): in JWT-only mode with `jwk_url`, upstream never loads keys. [fix: refresh in both modes]
- **Event triggers on a newly attached source DB** are only created on the next metadata call (07). [fix: create at startup]
- **Startup unlock of all scheduled events** duplicates deliveries when several instances autoscale (07). [fix: unlock only stale locks]
- **`get_scheduled_events` order_by is spliced into SQL unescaped** (07). [fix: allow-list column names]
- **Async action ownership check** missing when the action has no relationships (06); async action subscriptions never cleaned up (09). [fix]
- **`add_headers` in transforms appends duplicates** (06). [copy]
- **Metadata change on one instance closes only that instance's WebSockets** (09). [fix: close on schema sync too]
- **`use_prepared_statements` defaults** differ between env-configured and metadata sources (03, 01). [copy]
- **Explain SQL text parity** (03): the explain tests compare SQL text. [aim for the same SQL shape; skip exact-text explain tests initially]
- **Postgres `COMMENT` → description** (02): v2.51.0 imports table/column/computed-field comments when the metadata comment is absent; master no longer does. [follow v2.51.0: import them]
- **`/v1/version` string** (01, 11): the CLI uses it for feature checks. [report an upstream-compatible semver such as `v2.51.0`, with our own identity elsewhere]
- **Cloud Run / Cloudflare deviations** (07, 09, 11): honour `PORT`, events only process while an instance is up (recommend a separate always-on worker or a tick endpoint), `pg_dump` returns `not-supported` where no binary exists.

## Verification status

| Doc | Written | Reviewed against source |
|-----|---------|-------------------------|
| 01 | yes | yes (~55 corrections) |
| 02 | yes | yes (~28 corrections; checked against the v2.51.0 tag too) |
| 03 | yes | yes (16 edits) |
| 04, 05 | yes | yes (~37 corrections) |
| 06 | yes | yes (17 corrections; Kriti checked against kriti-lang 0.3.3) |
| 07 | yes | yes (~16 corrections) |
| 08 | yes | yes (~17 corrections) |
| 09 | yes | yes (~12 corrections) |
| 10, 12 | yes | yes (~22 edits) |
| 11, 13, 14 | yes | yes (~60 corrections; flag set checked against v2.51.0) |

The remaining "(inferred)" items should be settled by running the same request against
`hasura/graphql-engine:v2.51.0-ce` and recording the response as a golden file (plan in 14).
