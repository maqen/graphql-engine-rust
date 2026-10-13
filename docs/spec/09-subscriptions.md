# 09 — Subscriptions and the WebSocket transport

Status: research spec for the Rust drop-in replacement of Hasura GraphQL Engine V2 (OSS/CE behaviour first).
Ground truth: the Haskell source at the checked-out upstream revision (commit `895e7e9`, Oct 2026). All
paths below are relative to the upstream repo root. Anything marked **(inferred)** was not verified by
running the engine; it is reasoned from code or library behaviour.

Primary sources:

- `server/src-lib/Hasura/GraphQL/Transport/WebSocket.hs` — handlers `onConn`, `onMessage`, `onStart`, `onStop`, `onConnInit`, `onClose`
- `server/src-lib/Hasura/GraphQL/Transport/WebSocket/Protocol.hs` — message types, JSON encodings, close codes
- `server/src-lib/Hasura/GraphQL/Transport/WebSocket/Server.hs` — connection lifecycle, send queue, keep-alive, init timeout, connection reaper, shutdown
- `server/src-lib/Hasura/GraphQL/Transport/WebSocket/Types.hs` — connection state
- `server/src-lib/Hasura/GraphQL/Transport/WSServerApp.hs` — per-subprotocol behaviour (`mkWSActions`)
- `server/src-lib/Hasura/GraphQL/Execute.hs` — `buildSubscriptionPlan`, subscription restrictions
- `server/src-lib/Hasura/GraphQL/Execute/Subscription/{Plan,State,Options,Poll/Common,Poll/LiveQuery,Poll/StreamingQuery}.hs` — multiplexing, pollers, cohorts
- `server/src-lib/Hasura/Backends/Postgres/Execute/Subscription.hs` — multiplexed SQL
- `server/src-lib/Hasura/Backends/Postgres/Instances/Execute.hs` — `pgDBLiveQuerySubscriptionPlan`, `pgDBStreamingSubscriptionPlan`, `testMultiplexedQueryTx`
- `server/src-lib/Hasura/Backends/Postgres/Translate/Select/Streaming.hs` — streaming SQL
- `server/src-lib/Hasura/GraphQL/Schema/SubscriptionStream.hs` — `_stream` field schema
- `server/src-lib/Hasura/GraphQL/Execute/Action/Subscription.hs` — async action subscriptions
- `server/src-lib/Hasura/Server/Init/Arg/Command/Serve.hs`, `Server/Init/Env.hs` — env vars/flags

---

## Scope and priority

| Area | Priority | Notes |
|---|---|---|
| WS endpoint on `/v1/graphql` with `graphql-ws` (Apollo legacy) subprotocol | **Essential** | Apollo Client `subscriptions-transport-ws`, urql legacy, Hasura console use this. Default when no/unknown subprotocol. |
| `graphql-transport-ws` subprotocol (enisdenjo `graphql-ws`) | **Essential** | Modern Apollo Client (`GraphQLWsLink`), urql, Relay. |
| `connection_init` auth (admin secret, JWT, webhook, unauthorized role) | **Essential** | Reuses HTTP auth (see auth spec). |
| Live queries with multiplexing (cohorts, batched SQL, hash-based dedup) | **Essential** | The core feature; performance-critical. |
| Queries and mutations over WS | **Essential** | Some clients route all ops over WS. |
| Keep-alive, init timeout, JWT expiry close, close codes | **Essential** | Clients depend on these for reconnect logic. |
| Streaming subscriptions (`<table>_stream`) | **Important** | On by default since v2.7 (hidden only with `hide_stream_fields`). |
| Async action subscriptions | **Important** | Used by projects with async actions (see actions spec). |
| `/v1alpha1/graphql` (legacy error format) and `/v1/relay`, `/v1beta1/relay` over WS | Rarely used | Same machinery, different error encoding / schema. |
| Close-all-websockets-on-metadata-change, connection reaper, per-connection send queue cap | Important (ops) | Behavioural parity for deploys. |
| `/v1/graphql/explain` for subscriptions, `/dev/subscriptions` dumps | Rarely used | Explain golden tests compare SQL text exactly. |
| `remove_empty_subscription_responses` experimental feature | Rare | Changes observable behaviour (see §7.6). |
| Prometheus subscription metrics, OpenTelemetry spans, operation limits, dynamic DB routing (connection templates), response caching | **EE / Cloud only** | Skip for CE; keep hooks. |
| MSSQL / BigQuery / Data Connector subscriptions | Out of scope (Postgres-first) | Citus/Cockroach variants noted where they differ. |

Non-goals (not supported by Hasura V2 either): subscriptions over HTTP (SSE / chunked), remote schema
subscriptions, remote relationships inside subscriptions, `@defer`/`@stream` directives.

---

## 1. Endpoint and handshake

### 1.1 Paths

WebSocket upgrades are accepted on any HTTP path by the WAI layer (`websocketsOr` in
`server/src-lib/Hasura/Server/App.hs:864`), then `onConn.checkPath` (`WebSocket.hs:361`) dispatches on the
exact request path:

| Path | Error response type | Schema |
|---|---|---|
| `/v1/graphql` | GraphQL-compliant (`{"message","extensions":{"path","code"}}`) | Hasura (`QueryHasura`) |
| `/v1alpha1/graphql` | Legacy (`{"path","error","code"}`) | Hasura |
| `/v1/relay` | GraphQL-compliant | Relay |
| `/v1beta1/relay` | GraphQL-compliant | Relay |

Any other path → upgrade **rejected** with HTTP `404 Not Found`, body (GraphQL-compliant single error object):

```json
{"message":"only '/v1/graphql', '/v1alpha1/graphql', '/v1/relay' and '/v1beta1/relay' are supported on websockets","extensions":{"path":"$","code":"not-found"}}
```

Note: the `websockets` `requestPath` includes the query string, so `/v1/graphql?foo=1` is rejected
**(inferred)**. The Rust implementation may match on the path component only (strictly more lenient; no client
relies on the rejection).

If the server is shutting down: reject with `503 Service Unavailable`, header `Retry-After: 0`, body
`Server is shutting down` (plain text) (`Server.hs:667`).

### 1.2 CORS on upgrade (`onConn.enforceCors`, `WebSocket.hs:372`)

Only applied when an `Origin` header is present:

- CORS allow-all (default `*`): accept, keep all headers.
- CORS disabled (`HASURA_GRAPHQL_DISABLE_CORS=true`): accept; **strip the `Cookie` header** unless
  `HASURA_GRAPHQL_WS_READ_COOKIE=true` (`--ws-read-cookie`). An info log with the note
  "Cookie is not read when CORS is disabled, because it is a potential security issue. ..." is emitted.
- Allowed-origins list (`HASURA_GRAPHQL_CORS_DOMAIN`): origin must be in the FQDN list or match a wildcard
  entry; otherwise reject with HTTP `400`, body
  `{"message":"received origin header does not match configured CORS domains","extensions":{"path":"$","code":"access-denied"}}`.

The headers kept for the connection are all upgrade-request headers minus `sec-websocket-key`,
`sec-websocket-version`, `upgrade`, `connection` (`filterWsHeaders`).

### 1.3 Subprotocol selection (`Server.hs:614`, `Protocol.hs:52`)

- Look at `Sec-WebSocket-Protocol` request headers. If there is **exactly one** such header and its value is
  exactly the string `graphql-transport-ws` → `GraphQLWS` protocol. In every other case (absent, multiple headers,
  `graphql-ws`, a comma list like `graphql-transport-ws, graphql-ws`, any other value) → `Apollo` protocol.
- The server **always** responds with `Sec-WebSocket-Protocol: graphql-ws` (Apollo) or
  `Sec-WebSocket-Protocol: graphql-transport-ws` (GraphQLWS), even if the client did not request any
  subprotocol (`mkAcceptRequest`, `WSServerApp.hs`).

Rust guidance: replicate the selection rule, but (a) parse comma-separated values and pick
`graphql-transport-ws` if offered first, else `graphql-ws`; (b) only echo a subprotocol the client offered
(browsers fail the handshake if the server selects an unoffered subprotocol — RFC 6455 §4.1). Both are strictly
more compatible. Document the deviation.

Optional permessage-deflate compression: enabled only when `HASURA_GRAPHQL_CONNECTION_COMPRESSION=true`
(`--websocket-compression`), default off (`Serve.hs:1050`).

### 1.4 Per-connection tasks (`createServerApp.onAccept`, `Server.hs:678`)

On accept the server creates a connection id `WSId` (random UUIDv4, logged as `websocket_id`) and starts five
concurrent tasks; if any ends, the others are cancelled:

1. **rcv** — reads frames and processes **messages strictly serially** in arrival order (a long query on the
   connection blocks processing of later messages, including `stop`).
2. **send** — drains the main send queue, else the keep-alive slot (§1.6).
3. **keepAlive** — sends a keep-alive every `HASURA_GRAPHQL_WEBSOCKET_KEEPALIVE` seconds, starting
   **immediately on accept** (before `connection_init`).
4. **onJwtExpiry** — waits until the connection is initialised with a token expiry, sleeps until that time,
   then returns, which tears down the connection (§3.4).
5. **connInitTimer** — `HASURA_GRAPHQL_WEBSOCKET_CONNECTION_INIT_TIMEOUT` seconds (§3.2).

Each data frame is one JSON message. The server reads with `WS.receiveData` (`Server.hs`, `rcv`), which in
the `websockets` library returns the payload of **both text and binary** data messages, so a binary frame
containing JSON is processed the same way **(inferred from library semantics)**. The server always sends text
frames (`sendTextData`). Accept both in Rust.

Note on the init phase (`Server.hs:778-808`): until `connection_init` succeeds (or the init timer fires) the
supervising thread waits only on the init-timer TMVar and does **not** react to the other tasks ending. If the
client disconnects before a successful init, resources are only freed when the init timeout fires (not
observable by clients; the Rust engine should clean up immediately).

### 1.5 Environment variables / flags

| Env var | Flag | Default | Meaning |
|---|---|---|---|
| `HASURA_GRAPHQL_WEBSOCKET_KEEPALIVE` | `--websocket-keepalive` | `5` (seconds, non-negative int) | Interval of `ka` (Apollo) / `ping` (graphql-ws) messages. |
| `HASURA_GRAPHQL_WEBSOCKET_CONNECTION_INIT_TIMEOUT` | `--websocket-connection-init-timeout` | `3` (seconds, non-negative int) | Time allowed for a successful `connection_init`. Applies to **both** protocols despite the code comment. |
| `HASURA_GRAPHQL_WEBSOCKET_QUEUE_SIZE` | `--websocket-queue-size` | `300` (positive int) | Max queued *data* messages per connection before the producing operation is cancelled (§1.6). Present in recent versions only. |
| `HASURA_GRAPHQL_CONNECTION_COMPRESSION` | `--websocket-compression` | `false` | permessage-deflate. |
| `HASURA_GRAPHQL_WS_READ_COOKIE` | `--ws-read-cookie` | `false` | Keep `Cookie` when CORS is disabled. |
| `HASURA_GRAPHQL_CLOSE_WEBSOCKETS_ON_METADATA_CHANGE` | `--disable-close-websockets-on-metadata-change` | `true` | Close all sockets with 1012 after a metadata API change (§3.6). |
| `HASURA_GRAPHQL_LIVE_QUERIES_MULTIPLEXED_REFETCH_INTERVAL` | `--live-queries-multiplexed-refetch-interval` | `1000` (ms) | Sleep between live-query polls (§6.6). |
| `HASURA_GRAPHQL_LIVE_QUERIES_MULTIPLEXED_BATCH_SIZE` | `--live-queries-multiplexed-batch-size` | `100` | Cohorts per SQL execution. |
| `HASURA_GRAPHQL_STREAMING_QUERIES_MULTIPLEXED_REFETCH_INTERVAL` | `--streaming-queries-multiplexed-refetch-interval` | `1000` (ms) | Same for streaming pollers. |
| `HASURA_GRAPHQL_STREAMING_QUERIES_MULTIPLEXED_BATCH_SIZE` | `--streaming-queries-multiplexed-batch-size` | `100` | Same for streaming pollers. |
| `HASURA_GRAPHQL_EXPERIMENTAL_FEATURES` | | — | `hide_stream_fields` removes `_stream` fields; `remove_empty_subscription_responses` (§7.6); `streaming_subscriptions` is accepted but a no-op today **(inferred: no code path reads `EFStreamingSubscriptions`)**. |

Parsing rules (`Server/Init/Env.hs:318-349`): refetch interval is parsed as an integer number of
**milliseconds** (error `"refetch interval should be a non negative integer"`); batch size must be a
non-negative integer (`"batch size should be a non negative integer"`); keep-alive and init timeout are integer
seconds (init timeout error `"WebSocket Connection Timeout must not be negative"`). Note the
`SubscriptionsOptions` JSON (used in poller logs and `/dev/subscriptions`) is
`{"batch_size": <int>, "refetch_delay": <seconds as number>}` — the refetch delay is serialised in **seconds**
(e.g. `1` for the 1000 ms default) (`Options.hs` TODO comment).

A batch size of `0` is accepted by the parser but would make `chunksOf 0` misbehave **(inferred)**; the Rust
engine should clamp to ≥ 1.

Options are re-read on every poll iteration (`getSubscriptionOptions` reads the current `AppContext`), so a
config reload takes effect without restarting pollers.

### 1.6 Outgoing message queue (`Server.hs:212-323`)

- Each connection has an unbounded FIFO "main queue" plus a counter, and a separate **single-slot keep-alive
  slot**.
- *Data* messages (subscription results, query/mutation results — anything sent via `sendMsgWithMetadata`) are
  enqueued only if `count < HASURA_GRAPHQL_WEBSOCKET_QUEUE_SIZE`; otherwise the data is **dropped and the
  operation is cancelled** (below).
- *Control* messages (`connection_ack`, `error`, `complete`, `pong`, `connection_error`) always enqueue.
- Keep-alives overwrite the slot (coalesce) and are sent only when the main queue is empty.
- The sender writes one frame per message, in queue order.
- Exception: messages sent together with a close frame via `sendCloseWithMsg` with a message
  (`sendMsgAndCloseConn`, `Server.hs`) are written **directly** to the socket, bypassing the queue: the Apollo
  `connection_error` on init failure and the graphql-ws extra `error` on start failure (§4.4). They can
  therefore overtake queued messages. The Apollo `connection_error` for a *parse* failure goes through the queue.

**Over-capacity cancellation** (`cancelOperationOnOvercapacity`, `WebSocket.hs:1018`): when a data message for
operation `id` cannot be enqueued:

1. If it is a live/streaming subscription, atomically remove it from the connection's operation map and stop its
   subscriber (only the first claimant notifies; stragglers are silent).
2. Send an `error` message then a `complete` message for that `id`. The error is
   `err500 Unexpected` with message:
   - live query / query / mutation: `"This operation's result could not be delivered because the connection's send queue was at capacity, so it has been stopped. Please retry."`
   - streaming: `"... so it has been stopped. Please retry, using this cursor as the initial_value to resume from exactly where results were lost: {\"id\":\"42\"}."` and, for GraphQL-compliant endpoints, `extensions` replaced by `{"path":"$","code":"unexpected","resume_cursor":{"id":"42"}}` (the cursor in effect *before* the poll that produced the lost batch; values are text-encoded).

Error payload shapes follow §4.4 (`sendError`-style: Apollo `{"errors":[...]}`, graphql-ws `[...]`, v1alpha1 legacy object).

---

## 2. Message formats

### 2.1 Common parsing (`Protocol.hs` `instance FromJSON ClientMsg`)

The server uses **one parser for both protocols**; the subprotocol only changes what the server *sends*.
Every client message must be a JSON object with a string `type`:

| `type` | Parsed as | Required fields |
|---|---|---|
| `connection_init` | `CMConnInit` | optional `payload` (object `{"headers": {string: string}}`; other keys ignored; `payload` may be absent/null) |
| `start` | `CMStart` | `id` (string), `payload` (GraphQL request) |
| `subscribe` | `CMStart` (identical to `start`) | `id`, `payload` |
| `stop` | `CMStop` | `id` (string) |
| `complete` | `CMStop` (identical to `stop`) | `id` |
| `connection_terminate` | `CMConnTerm` | — |
| `ping` | `CMPing` | optional `payload` (`{"message": string?}`) |
| `pong` | `CMPong` | optional `payload` |
| anything else | parse failure: `unexpected type for ClientMessage: <type>` | |

- `id` must be a JSON string (a number fails parsing). Unknown extra fields are ignored.
- GraphQL request `payload`: `{"query": string, "variables": object|null, "operationName": string|null}`
  (`GQLReq`, camelCase keys; `query` required). Batched (array) payloads are not supported. `extensions` is
  ignored.
- `connection_init.payload.headers` values must all be strings, else the whole message fails to parse.
- So an Apollo client may send `subscribe`/`complete`/`ping`, and a graphql-ws client may send
  `start`/`stop`/`connection_terminate`; they behave identically.

**Parse failure** of any frame (invalid JSON, unknown type, missing `id`, ...): error text
`"parsing ClientMessage failed: " <> aesonError` (e.g. `parsing ClientMessage failed: Error in $: key "id" not found`
— the aeson wording need not be byte-identical) and:

- Apollo: send `{"type":"connection_error","payload":"parsing ClientMessage failed: ..."}`; **connection stays open**.
- graphql-ws: close with code **4400**, reason `"4400: Parsing client message failed: parsing ClientMessage failed: ..."` (JSON-wrapped, §2.4); no message sent.

### 2.2 Apollo protocol (`graphql-ws` subprotocol, subscriptions-transport-ws)

Server → client messages (`encodeServerMsg`, key order `type`, `id`, `payload`):

```json
{"type":"connection_ack"}
{"type":"ka"}
{"type":"connection_error","payload":"<error text>"}
{"type":"data","id":"<id>","payload":{"data":{...}}}
{"type":"data","id":"<id>","payload":{"data":null,"errors":[{...}]}}
{"type":"error","id":"<id>","payload":<error payload, see §4.4>}
{"type":"complete","id":"<id>"}
{"type":"pong"}                                   // or with "payload" echoed, only if client sent ping
```

`connection_ack` carries **no payload**. `ka` carries no payload. Data payloads are `{"data": <raw JSON>}` on
success, `{"data":null,"errors":[...]}` on execution error (`encodeGQResp`, `HTTP/Protocol.hs:228`).

Typical flows:

```
C: {"type":"connection_init","payload":{"headers":{"x-hasura-admin-secret":"s"}}}
S: {"type":"connection_ack"}
S: {"type":"ka"}                                  // immediately after ack, then every 5 s
C: {"id":"1","type":"start","payload":{"query":"subscription { t { id } }"}}
S: {"type":"data","id":"1","payload":{"data":{"t":[]}}}
...                                               // only when result changes
C: {"id":"1","type":"stop"}
                                                  // no message is sent in response to stop
C: {"type":"connection_terminate"}
S: <close 1000 "GQL_CONNECTION_TERMINATE received">
```

Query/mutation over WS (Apollo): `data` then `complete`.

### 2.3 graphql-transport-ws protocol (enisdenjo `graphql-ws`)

```json
{"type":"connection_ack"}
{"type":"ping","payload":{"message":"keepalive"}}     // server keep-alive, every 5 s
{"type":"pong"}                                       // reply to client ping; echoes client payload if any
{"type":"next","id":"<id>","payload":{"data":{...}}}
{"type":"next","id":"<id>","payload":{"data":null,"errors":[...]}}   // live-query poll errors (§8)
{"type":"error","id":"<id>","payload":[{...},...]}    // array of GraphQL errors
{"type":"complete","id":"<id>"}
```

- `PingPongPayload` has a single optional field `message` (`_smMessage` → `message`). Client `ping` with payload
  `{"message":"x"}` is answered `{"type":"pong","payload":{"message":"x"}}`; other payload keys are dropped; ping
  without payload → `{"type":"pong"}`.
- Client `pong` messages are ignored; the server never times out a client for not answering pings.
- Server keep-alive pings start immediately at accept (before `connection_init`), then every interval
  **(verified in code; deviation from graphql-ws spec which only allows ping after ack is benign)**.

Hasura deviations from the graphql-ws spec that must be preserved for drop-in parity (clients tolerate them):

| Situation | graphql-ws spec | Hasura V2 actual |
|---|---|---|
| `error` for an operation | terminal, no `complete` follows | `error` is **followed by `complete`** (§4) |
| `subscribe` before ack | close 4401 Unauthorized | `error` (start-failed) + close **1000** (§4.2) |
| duplicate `subscribe` id | close 4409 `Subscriber for <id> already exists` | `error` (start-failed) + close **1000** |
| second `connection_init` | close 4429 Too many initialisation requests | re-authenticates, sends another `connection_ack` (§3.1) |
| init timeout | close 4408 | close 4408 (matches) |
| invalid message | close 4400 | close 4400 (matches) |
| auth failure | close 4403 Forbidden | close 4403 (matches) |

`ServerErrorCode` also defines 1002, 4401, 4409, 4429 but **nothing in OSS uses them** (`Protocol.hs:225`).

### 2.4 Close frames

`sendCloseWithMsg` (`WebSocket.hs:261`) sends a close frame whose **reason is JSON**:
`encodeServerErrorMsg` encodes `ServerErrorMsg {unServerErrorMsg :: Text}` with `hasuraJSON`
(`hasuraJSON = aesonPrefix snakeCase`, `lib/hasura-prelude/src/Hasura/Prelude.hs:414`), i.e. exactly
`{"server_error_msg":"4400: ..."}`. The key is determined by the same generic-derivation rule that maps
`_cpHeaders` → `headers` (connection_init payload) and `_smMessage` → `message` (ping payload): drop the
leading lowercase prefix (`un`), snake-case the rest (`ServerErrorMsg` → `server_error_msg`); a single-field
record is still encoded as an object (aeson `unwrapUnaryRecords` defaults to false). No upstream test asserts
the reason text, and no known client parses it (clients only look at the close *code*). Emit it compactly
(no spaces), e.g. `{"server_error_msg":"4403: Forbidden"}`. Messages:

| Code | Reason text (inside the JSON) |
|---|---|
| 4400 | `4400: <msg>` |
| 4403 | `4403: Forbidden` |
| 4408 | (not via this path; see below) |

Close frames sent via `closeConnWithCode` carry a **plain-text** reason:

| Code | Reason | When |
|---|---|---|
| 1000 | `GQL_CONNECTION_TERMINATE received` | client `connection_terminate` |
| 4408 | `Connection initialisation timed out` | graphql-ws init timeout |
| 1011 | `Connection initialisation timed out` | Apollo init timeout |
| 1012 | `Server state changed, restarting the server` | metadata changed via this instance's metadata API (§3.6) |
| 1012 | `shutting server down` | graceful shutdown |
| 1012 | `cors policy changed`, `auth mode changed`, `allow list enabled`, `allow list updated`, `HASURA_GRAPHQL_STRINGIFY_NUMERIC_TYPES env var changed`, `HASURA_GRAPHQL_V1_BOOLEAN_NULL_COLLAPSE env var changed`, `'bigquery_string_numeric_input' removed/added in HASURA_GRAPHQL_EXPERIMENTAL_FEATURES env var`, `'hide-aggregation-predicates' removed/added in HASURA_GRAPHQL_EXPERIMENTAL_FEATURES env var`, `'hide-stream-fields' removed/added in HASURA_GRAPHQL_EXPERIMENTAL_FEATURES env var`, `naming convention has been changed` | connection reaper (§3.6) |

RFC 6455 limits close reasons to 123 bytes. Hasura's 4400 reasons can exceed that (the Haskell `websockets`
library does not truncate **(inferred)**). The Rust implementation must truncate the reason to ≤ 123 bytes
on a UTF-8 boundary (tungstenite rejects longer reasons).

---

## 3. Connection lifecycle and authentication

### 3.1 `connection_init` (`onConnInit`, `WebSocket.hs:1335`)

Connection state machine (`WSConnState`):

```
CSNotInitialised(upgradeHeaders, ip)
   └─ connection_init ok   → CSInitialised{userInfo, tokenExpiry, reqHeaders=payloadHeaders, ip}
   └─ connection_init fail → CSInitError(errorText)
CSInitialised ─ connection_init again → re-authenticate (see below)
CSInitError   ─ connection_init again → connection_error/close with the stored error text
```

Authentication input headers = `payload.headers` (names case-insensitive) **followed by** the upgrade-request
headers (after CORS filtering): `mkHeaders st = paramHeaders ++ getClientHdrs st` (`WebSocket.hs`, `onConnInit`).
These go through the normal `resolveUserInfo` (admin secret, `Authorization: Bearer` JWT, auth webhook,
unauthorized role, `x-hasura-role` selection — see auth spec). `x-hasura-*` session headers in `payload.headers`
behave exactly like HTTP request headers.

**Precedence with duplicate header names is not uniform** (verified in code; it follows from which lookup each
auth step uses on the concatenated list `payload ++ upgrade`):

| What | Lookup | Winner on duplicate |
|---|---|---|
| `x-hasura-*` session variables, incl. `x-hasura-admin-secret` check and `x-hasura-role` in admin-secret / no-auth / unauthorized-role modes | `mkSessionVariablesHeaders` = `HashMap.fromList` (`Authentication/Session.hs:175`) — last occurrence wins | **upgrade header** |
| `x-hasura-role` in JWT mode | `getRequestHeader` = `find` (first match) (`Server/Auth/JWT.hs`, `withAuthZ`) | **payload header** |
| JWT token (`Authorization`, `Cookie`, custom header) | all occurrences collected (`keyTokensOnAuthTypes`) | both are tried; if two tokens match the JWT config → `400 invalid-headers` `Could not verify JWT: Multiple JWTs found` |
| `x-request-id`, headers forwarded to actions/remote schemas | stored request headers = payload headers only | payload (upgrade headers not used) |
| Auth webhook GET | all headers forwarded (minus `commonClientHeadersIgnored`) | both duplicates sent |
| Auth webhook POST body `headers` | `HashMap.fromList` of header text pairs | last (upgrade), when names are spelled with identical case |

In practice browsers cannot set custom upgrade headers (only `Cookie`/`Origin`), so collisions mostly arise
with cookie-based JWT plus an `Authorization` payload header, or non-browser clients. The Rust port should
reproduce this table exactly (simplest: build the same concatenated list and reuse the HTTP auth functions with
the same lookup semantics).

On success:
1. state ← `CSInitialised` with `tokenExpiry` = JWT `exp` (JWT mode) or, in webhook mode, the expiry derived from
   the `Cache-Control` (`max-age`, preferred) or `Expires` **keys of the webhook's JSON response body** (not its
   HTTP response headers; exact-case keys; `Server/Auth/WebHook.hs`, `getUserInfoFromHdrs`), else none
   (admin secret / unauthorized role → no expiry);
2. mark initialised (cancels the init timer);
3. send `connection_ack`;
4. immediately send one keep-alive (`ka` / `ping`).

On failure (error text = QErr message, e.g. `invalid "x-hasura-admin-secret"`,
`Could not verify JWT: JWTExpired`, `"x-hasura-admin-secret" required, but not found`,
`Authentication hook unauthorized this request` — the double quotes are part of the message, produced by
`<>>`/`<<>` + `dquote` in `Server/Auth.hs`):
- state ← `CSInitError <message>`;
- Apollo: send `{"type":"connection_error","payload":"<message>"}` directly, then close **4400** with reason
  `4400: Connection initialization failed: <message>`;
- graphql-ws: close **4403** reason `4403: Forbidden` (no message).

Second `connection_init` on an initialised connection: auth is re-run but with **only the payload headers**
(upgrade headers such as `Cookie` are no longer included — `getClientHdrs` returns `[]` once initialised);
on success the state is replaced and another `connection_ack` is sent. The JWT-expiry timer is **not** reset
(it already captured the first expiry; §3.4).

`reqHeaders` stored for the connection (forwarded to actions / remote-schema joins / used for
`x-request-id`) are the **payload headers only**, not the upgrade headers.

Request id: per operation, `x-request-id` from the payload headers if present (then every operation on the
connection reuses it), else a fresh id (`getRequestId`, `Server/Types.hs:67`).

### 3.2 Connection init timeout (`Server.hs:778-808`)

If no successful `connection_init` within `HASURA_GRAPHQL_WEBSOCKET_CONNECTION_INIT_TIMEOUT` (default 3 s):
graphql-ws → close `4408 "Connection initialisation timed out"`; Apollo → close
`1011 "Connection initialisation timed out"`. A *failed* init does not stop the timer, but failed inits close
the socket anyway (both protocols). Log event `connection_timeout`.

### 3.3 Keep-alive

Every `HASURA_GRAPHQL_WEBSOCKET_KEEPALIVE` seconds (default 5) from accept: Apollo `{"type":"ka"}`, graphql-ws
`{"type":"ping","payload":{"message":"keepalive"}}`. Keep-alives never block data (coalesced slot). There is no
server-side dead-client detection beyond TCP errors.

### 3.4 Token expiry

`tokenExpiryHandler` (`WebSocket.hs:318`): once the state is `CSInitialised` with an expiry, sleep until the
expiry; when it fires the connection handler exits (log `jwt_expired`), all operations are stopped (`onClose`),
and the TCP connection is dropped. No WebSocket close frame or `connection_error`/`complete` is sent.
Code path (verified, `Server.hs` `onAccept`): `onJwtExpiry` returns → `LA.waitAnyCancel` yields `Right` → log
`EJwtExpired` → the other four tasks (incl. the sender, so any still-queued messages are discarded) are
cancelled → `bracket` cleanup runs `onConnClose` (remove from conn map, `onClose` stops all operations) → the
server app returns. There is **no** `closeConn`/`sendCloseCode` call on this path (contrast with the init-timeout
branch, which calls `closeConnWithCode`). That returning from the app then just closes the underlying stream
without a close frame is library behaviour (`wai-websockets` closes the stream in its `bracket`)
**(inferred, library not in the source tree)** — clients observe an abnormal closure (1006) and reconnect with
a fresh token (Apollo `connectionParams` function / graphql-ws `connectionParams` are re-evaluated on
reconnect). Expiry also applies to webhook-mode expiries (§3.1), not only JWT. The timer only starts once the
connection is initialised with an expiry; it is computed once (`STM.retry` until an expiry exists).

Rust guidance: close the socket at expiry. Do **not** use 4401/4403 (graphql-ws clients treat those as fatal
and stop retrying, which would differ from V2). Either abort the TCP connection (exact parity) or send close
code `1011`/`1006`-equivalent abort; recommended: abort (parity). If a client re-inits with a fresh token,
Hasura still closes at the *first* expiry; the Rust engine may reschedule on re-init (benign improvement), but
document it.

Only operations started before expiry exist; there is no per-operation re-check of the token.

### 3.5 Stop, terminate, close (`onStop`, `onClose`, `claimAndStopSubscription`)

- `stop`/`complete` with an id: atomically remove the id from the connection's operation map; if it was a live
  or streaming subscription, remove the subscriber from its cohort/poller (stopping the poller if it was the
  last). **No message is sent back** (neither protocol). Unknown id (incl. finished queries/mutations): debug
  log only.
- Async action subscriptions *without relationships* are **not** in the operation map, so `stop` does not stop
  them (§9).
- `connection_terminate`: close 1000 `GQL_CONNECTION_TERMINATE received` (either protocol).
- On socket close (any reason): every tracked operation is stopped as above; no messages.
- In-flight polls that complete after removal must not send anything: `onChange` re-checks that the id is
  still in the operation map before enqueueing (`WebSocket.hs:1166`).

### 3.6 Server-initiated closes of all connections

1. **Metadata change** (`Server/API/Metadata.hs:193`): after any metadata API call that modifies metadata on
   *this* instance, if `HASURA_GRAPHQL_CLOSE_WEBSOCKETS_ON_METADATA_CHANGE` is enabled (default), all sockets on
   this instance are closed with `1012 "Server state changed, restarting the server"`. Other instances that
   pick up the change via schema sync do **not** close their sockets (only `Metadata.hs` triggers it), so their
   existing subscriptions keep running with the *old* plan (old permissions) until clients reconnect.
   `/v2/query` `run_sql` that changes the schema without a metadata call does not close sockets; neither do
   legacy `/v1/query` metadata-style calls (the close is only wired into `Server/API/Metadata.hs`, i.e.
   `/v1/metadata`). The closer only sends the 1012 close frame (`forceConnReconnect`) and removes the
   connection from the map; per-connection tasks end when the client's close reply / disconnect arrives.
   Errors `ConnectionClosed` while closing are ignored.
2. **Connection reaper** (`websocketConnectionReaper`, `Server.hs:461`): a background loop every 1 s compares
   security-sensitive config with the last seen values and closes **all** sockets with 1012 and the reason
   listed in §2.4 when any of these change: CORS policy, auth mode, allowlist enabled (disabled→enabled), allowlist
   contents (while enabled), stringify-numerics, boolean-null-collapse, bigquery numeric input, experimental
   features `hide_aggregation_predicates` / `hide_stream_fields`, default naming convention.
3. **Shutdown**: status → shutting down (new upgrades get 503), all sockets closed with
   `1012 "shutting server down"`.

1012 ("service restart") is the signal for clients to reconnect after a short backoff.

---

## 4. Operations over the socket (`onStart`, `WebSocket.hs:442`)

For every `start`/`subscribe`:

1. If the id already exists in the operation map (only subscriptions are tracked) → **start error**
   `"an operation already exists with this id: <id>"`.
2. If the connection is `CSInitError e` → start error `"cannot start as connection_init failed with: <e>"`;
   if `CSNotInitialised` → start error `"start received before the connection is initialised"`.
3. Parse/validate/allowlist (`checkGQLExecution`), select single operation (`getSingleOperation`), build the
   execution plan exactly as for HTTP (same parser, same permissions, same session from the connection).
   Pre-execution errors → **pre-exec error** (§4.4).
4. Execute by operation type (§4.1–4.3).

### 4.1 Queries

Executed like HTTP (DB steps, remote schemas, actions, introspection, remote joins). Response headers are
dropped. Result → one data message (`data`/`next`) with `{"data":{...}}` (alias order preserved), then
`complete`. Execution errors → post-exec error (§4.4) then `complete`. Queries are not tracked in the
operation map. The EE response cache (`@cached`) is consulted but irrelevant for CE.

### 4.2 Mutations

Same as HTTP including the Postgres transaction coalescing of multiple root mutation fields on one source
(see mutations spec). One data message then `complete`. Read-only mode rejections, etc. are pre/post-exec errors
as on HTTP.

### 4.3 Subscriptions

Plan built by `buildSubscriptionPlan` (§5). Then:
- live query → `startLiveQuery` (validates & registers a subscriber; §6), id inserted into operation map;
- streaming → `startStreamingQuery` (§7);
- async action (no relationships) → registered in the global async-action map (§9).

No message is sent on successful start; the first data arrives from the poller.

### 4.4 Error payload shapes

Let `E` be the QErr. Encoders: GraphQL-compliant endpoints (`/v1/graphql`, relay) use
`encodeGQLErr HideInternalErrors E` = `{"message": msg, "extensions": {"path": "$...", "code": code}}`
(if the QErr carries extra `extensions` from an action/webhook, those replace `{path,code}`); `/v1alpha1/graphql`
uses legacy `encodeQErr` = `{"path": "$", "error": msg, "code": code}` (internal field removed). Internal error
details are **always hidden on websockets** (`HideInternalErrors` hard-coded), regardless of
`HASURA_GRAPHQL_DEV_MODE`/admin.

| Kind | Apollo (`/v1/graphql`) | graphql-ws (`/v1/graphql`) | `/v1alpha1/graphql` (Apollo) |
|---|---|---|---|
| **Start error** (`sendStartErr`, code `start-failed`, status 400) | `{"type":"error","id":I,"payload":{"message":M,"extensions":{"path":"$","code":"start-failed"}}}` then `{"type":"complete","id":I}`; socket stays open | same `error` (queued), then a directly-written `{"type":"error","id":I,"payload":"<M as JSON string>"}`, then close **1000** with reason `4400: <M>` (JSON-wrapped). The queued `complete` is usually lost. Ordering of the two errors is not guaranteed. | `{"type":"error","id":I,"payload":{"path":"$","error":M,"code":"start-failed"}}` then `complete` |
| **Pre-exec error** (`sendError`: parse/validation/permission/plan/validation-query failures) | `{"type":"error","id":I,"payload":{"errors":[E]}}` then `complete` | `{"type":"error","id":I,"payload":[E]}` then `complete` | `{"type":"error","id":I,"payload":E_legacy}` then `complete` |
| **Post-exec error** (query/mutation execution failed) | `{"type":"data","id":I,"payload":{"data":null,"errors":[E,...]}}` then `complete` | `{"type":"error","id":I,"payload":[E,...]}` then `complete` | `data` with `{"data":null,"errors":[E_legacy]}` then `complete` |
| **Live-query poll error** | `{"type":"data","id":I,"payload":{"data":null,"errors":[E]}}` repeated each poll; subscription stays active | `{"type":"next","id":I,"payload":{"data":null,"errors":[E]}}` repeated; stays active | `data` with GraphQL-compliant `E` (poller always uses `encodeGQLErr`) |

Example (from `tests-py/test_subscriptions.py::test_double_alias`):

```json
{"type":"error","id":"2","payload":{"errors":[{"extensions":{"path":"$","code":"validation-failed"},"message":"subscriptions must select one top level field"}]}}
```

(Key order inside `E` is `message`, `extensions` per `encodeGQLErr`; JSON key order is not significant.)

Example from `queries/subscriptions/basic/negative_test.yaml` (role `user`, missing session variable): pre-exec
error `{"extensions":{"path":"$","code":"not-found"},"message":"missing session variable: \"x-hasura-custom\""}`.

---

## 5. Subscription restrictions (`Execute.hs:438-495`, `buildSubscriptionPlan`)

Checked in this order; all are `400` QErrs delivered as pre-exec errors:

| Condition | Code | Message |
|---|---|---|
| Schema has no subscription root for the role | `validation-failed` | `no subscriptions exist` |
| More than one top-level selection (aliases count; `__typename` counts) and not (`@_multiple_top_level_fields` present **and** all fields in one namespace) | `validation-failed` | `subscriptions must select one top level field` |
| Remote schema field | `not-supported` | `subscription to remote server is not supported` |
| `__typename`/`__schema` etc. at the root, not inside a source namespace | `not-supported` | `Introspection at root field level is not supported over subscriptions` |
| Remote relationship anywhere in the selection (DB or action field) | `not-supported` | `Remote relationships are not allowed in subscriptions` |
| Permission filter (row filter) containing a remote-relationship predicate, at the root or any nested relationship | `not-supported` | `subscriptions on this field is not supported` |
| Synchronous (query-type) action | `not-supported` | `query actions cannot be run as a subscription` |
| `RFMulti` root | `not-supported` | `not supported over subscriptions` |
| Streaming field mixed with live fields | `not-supported` | `streaming and livequery subscriptions cannot be executed in the same subscription` |
| More than one streaming root field (only reachable via `@_multiple_top_level_fields`) | `not-supported` | `exactly one root field is allowed for streaming subscriptions` |
| Async action without relationships mixed with DB fields | `not-supported` | `async action queries with no relationships aren't expected to mix with normal source database queries` |
| Fields from different sources | `not-supported` | `all fields of a subscription must be from the same source` |
| Unbound session variable referenced by the plan | `not-found` | `missing session variable: "x-hasura-..."` |

Allowed subscription root fields per table (subject to select permission and its
`subscription_root_fields` list, see authz/schema specs): `<table>`, `<table>_by_pk`, `<table>_aggregate`,
`<table>_stream`; tracked functions / native queries / logical-model roots exposed in subscription root; async
action query fields `<action>(id: uuid!)`. Relay endpoints expose `<table>_connection` and `node`.

`@_multiple_top_level_fields` is an **unsupported test-only directive** (used by `validate.py` to run every
query fixture as a subscription); implement it so the py test-suite's `transport='subscription'` mode works.
Multiple live fields are then aggregated by `json_build_object` (§6.3).

Subscriptions over HTTP: `POST /v1/graphql` with a subscription → `400`
`{"errors":[{"extensions":{"path":"$","code":"unexpected-payload"},"message":"subscriptions are not supported over HTTP, use websockets instead"}]}`
(`Transport/HTTP.hs:513`).

---

## 6. Live queries (multiplexed polling)

### 6.1 Data model (`Execute/Subscription/Plan.hs`, `Poll/Common.hs`, `State.hs`)

```
SubscriptionsState
 ├─ liveQueryMap   : Map<PollerKey, Poller<()>>
 ├─ streamQueryMap : Map<PollerKey, Poller<TVar CursorValues>>
 └─ asyncActions   : Map<OperationId, AsyncActionQueryLive>       (global, keyed by client op id!)

PollerKey  = (sourceName, roleName, multiplexedSqlText, resolvedConnectionTemplate, parameterizedQueryHash)
Poller     = { cohorts: Map<CohortKey, Cohort>, state: Success|Error, thread, pollerId(uuid),
               operationNames: Map<Maybe opName, count> }
CohortKey  = CohortVariables = { session: filtered session vars, query: Map<varName, text>,
                                 synthetic: [text], cursor: Map<colName, text|null> }
Cohort     = { cohortId: uuid, previousResponseHash: Option<Hash>,
               existingSubscribers: Map<SubscriberId, Subscriber>, newSubscribers: Map<..>,
               streamCursor (streaming only) }
Subscriber = { subscriberId: uuid, metadata{websocket_id, operation_id, operation_name, request_id},
               requestId, operationName, onChange callback }
```

- **Poller** = one multiplexed SQL statement (same role, same SQL text, same source/connection, same
  parameterized query hash). The parameterized query hash is a hash of the normalized selection set (variables
  replaced), so two operations with the same shape share a poller; operation name does not matter.
- **Cohort** = subscribers with byte-identical variables (after filtering). One cohort = one row in the SQL
  result. Subscribers in a cohort share one result.
- Cohort key includes only the session variables **actually referenced** by the plan
  (`mkCohortVariables` filters by `_qpiReferencedSessionVariables`); if the whole session object is referenced
  (`UVSession`, e.g. a function with a session argument) all session variables are included. Test
  `test_extraneous_session_variables_are_discarded_from_query`: role `public` with no session-var permission →
  `variables.session == {}`; role `user` with `user_id = X-Hasura-User-Id` → `{"x-hasura-user-id":"1"}`
  (session variable names are lower-cased).

### 6.2 Parameterisation (`resolveMultiplexedValue`, `Postgres/Execute/Subscription.hs:478`)

While translating the IR, every SQL value is replaced by a lookup into the cohort's JSON variables row
`_subs.result_vars`:

| IR value | Collected into | SQL emitted |
|---|---|---|
| `UVParameter (FromGraphQL var) v` (values passed through `mkParameter` with variable origin, e.g. `_by_pk` args / function args) | `query[varName] = v` | `("_subs"."result_vars" #>> ARRAY['query', '<var>'])::<pgtype>` |
| any other `UVParameter` (literals and variables nested in bool-exp/input objects; `FreshVar`/`FromInternal`) | `synthetic` append, index = position | `(... #>> ARRAY['synthetic', '<i>'])::<pgtype>` |
| `UVSessionVar ty v` | referenced set += v (fails with `missing session variable: "<v>"` if absent) | `(... #>> ARRAY['session', '<v lowercased>'])::<ty>` |
| `UVSession` | referenced set = all session vars | `(... #>> ARRAY['session'])::json` |
| `UVLiteral` | — | inlined |

`#>>` returns text; the type annotation `::<type>` (with the scalar's constructor function for special types,
`withConstructorFn`, e.g. PostGIS geography/geometry built from GeoJSON — see SQL spec) converts back. Array
types are cast without constructor. Values are stored **text-encoded** (`TxtEncodedVal`: Postgres text form,
`null` for SQL NULL). Tests (`TestSubscriptionMultiplexingPostgres`):
`articles(where:{id:{_eq:$id}})` with `{"id":1}` → `variables.synthetic == ["1"]`; `_in: $ids` with `[1,2,3]` →
`synthetic == ["{1,2,3}"]` (Postgres array literal text).

Cohort variables JSON (`instance ToJSON CohortVariables`; used as the per-cohort element of `$2` and in
`/v1/graphql/explain` `variables`):

```json
{"session":{"x-hasura-user-id":"1"},"query":{"id":"5"},"synthetic":["1","{1,2,3}"],"cursor":{}}
```

### 6.3 Multiplexed SQL shape (`mkMultiplexedQuery`)

Logical shape (identifiers before Hasura's alias-renaming pass):

```sql
SELECT "_subs"."result_id", "_fld_resp"."root" AS "result"
FROM UNNEST(($1)::uuid[], ($2)::json[]) AS "_subs"("result_id", "result_vars")
LEFT OUTER JOIN LATERAL (
    SELECT json_build_object('<alias1>', "<alias1>"."root", '<alias2>', "<alias2>"."root") AS "root"
    FROM (<root-field select for alias1>) AS "<alias1>",
         (<root-field select for alias2>) AS "<alias2>"
) AS "_fld_resp" ON ('true')
```

- `$1` = `uuid[]` of cohort ids; `$2` = array of cohort variable JSON (sent as `jsonb[]` binary — CockroachDB
  workaround — and cast to `json[]` in SQL). Both arrays are positionally aligned.
- The keys of `json_build_object` are the root field **aliases** (response keys); each root select returns a
  single row with column `root` (`coalesce(json_agg(...), '[]')` for lists, single object/null for `_by_pk`,
  aggregate object, relay connection), produced by the normal select translator (SQL spec) with values replaced
  per §6.2.
- Custom SQL CTEs (native queries) are placed as inner CTEs of the lateral select.
- The final SQL goes through the same identifier renaming pass as all Postgres queries
  (`toQuery = renameIdentifiersSelectWithTopLevelCTE`), which prefixes table aliases with `_`, so the emitted
  text uses `"__subs"`, `"__fld_resp"`, `"_<alias>"`. Exact golden text from
  `tests-py/queries/explain/docs_subscription.yaml` (compared byte-for-byte by
  `test_documented_subscription`):

```sql
SELECT  "__subs"."result_id" , "__fld_resp"."root" AS "result" FROM UNNEST(($1)::uuid[], ($2)::json[]) AS "__subs"("result_id", "result_vars") LEFT OUTER JOIN LATERAL (SELECT  json_build_object('user', "_user"."root" ) AS "root" FROM  (SELECT  coalesce(json_agg("root" ), '[]' ) AS "root" FROM  (SELECT  row_to_json((SELECT  "_e"  FROM  (SELECT  "_root.base"."name" AS "name"       ) AS "_e"      ) ) AS "root" FROM  (SELECT  *  FROM "public"."user"  WHERE ('true')     ) AS "_root.base"      ) AS "_root"      ) AS "_user"      ) AS "__fld_resp" ON ('true')
```

  (for `subscription getUsers { user { name } }` as admin). Matching this whitespace exactly only matters for the
  explain golden test; the Rust SQL builder should reuse the SQL module's printer and renaming rules.
- Query tags (if configured on the source) are appended as for queries, with live-query attributes
  (`QTLiveQuery`: root field name + parameterized query hash) (`Execute.hs:153`).
- Execution: `runQueryTx` on the source pool (read-only, read-replica capable — see SQL/sources spec), as a
  prepared statement (`PG.withQE ... True`). Result rows decode to `(cohort_id uuid, result json-as-bytes)`.

### 6.4 Start-time validation (`pgDBLiveQuerySubscriptionPlan`)

Before registering, inside one **read-only** transaction on the source:

1. **Variable validation** (`validateVariablesTx`): if there are any query/synthetic values, run
   `SELECT ('v1')::t1, ('v2')::t2, ...` (each value as text literal with its type annotation). A Postgres
   *data exception* (SQLSTATE class 22) becomes the error (e.g. invalid input syntax for type) → pre-exec error.
   Purpose: one bad cohort must not break the whole multiplexed batch.
2. **Test run** (`testMultiplexedQueryTx`): execute the multiplexed SQL once with a single fresh cohort id and
   this cohort's variables, discarding rows (no `LIMIT`, deliberately, so function/computed-field runtime errors
   surface). Errors → pre-exec error.

Only then is the subscriber added. The cohort id generated here is reused only if this subscriber creates a new
cohort.

### 6.5 Registration (`addLiveQuery`, `State.hs`)

Atomically (one STM transaction):
- poller exists? → cohort with identical `CohortVariables` exists? → add subscriber to `newSubscribers`;
  else create cohort (`previousResponseHash = None`) with the subscriber; increment operation-name count.
- no poller → create poller with that cohort; then (outside the transaction) spawn the poller task and record
  its handle.

Then the subscription id is inserted into the connection's operation map.

### 6.6 Poll loop (`pollLiveQuery`, `Poll/LiveQuery.hs`)

Each poller task runs `forever { poll(); sleep(refetchInterval) }` — the **first poll runs immediately** when a
poller is created; a subscriber joining an existing poller waits up to one interval (+ poll time) for its first
message. The effective period is poll duration + interval (not a fixed-rate timer).

One poll:

1. **Snapshot**: list all cohorts; for each cohort atomically: read existing and new subscribers, move new →
   existing, clear new. Snapshot = (variables, hash ref, existing list, new list).
2. **Batch**: split cohort snapshots into chunks of `batch_size` (in map iteration order); batch ids 1..n.
3. **Execute batches concurrently** (`forConcurrently`; each batch uses its own pooled connection): run the
   multiplexed SQL with that batch's `(cohort_id, variables)` arrays.
4. For each result row `(cohort_id, bytes)` whose id is in the batch (unknown ids ignored):
   - `hash = BLAKE2b-256(bytes)` (hash of the **raw DB JSON bytes**, before namespace wrapping / `__typename`
     modifier).
   - if `hash != previousResponseHash`: store `hash`, push to **existing + new** subscribers;
     else push only to **new** subscribers.
   - push = for each subscriber concurrently call `onChange(Right {payload: applyModifier(bytes), execTime})`.
5. If the batch query **failed**: for every cohort in the batch, push
   `Left [encodeGQLErr HideInternalErrors err]` to existing + new subscribers (errors are always pushed, every
   poll) and set the cohort's stored hash to `None`, so the next success is always pushed.
   Poller state flips to `Error` (metrics only); next successful batch flips back.
6. Post-poll hook → `livequery-poller-log` (§11).

Cohorts with no row in the result (only possible with `remove_empty_subscription_responses`, §7.6) push
nothing, not even to new subscribers.

`onChange` on the websocket side (`WebSocket.hs:1166`): if the operation id is no longer in the connection's
operation map, drop; success → data message with payload `{"data": <bytes>}` (or
`{"data": {"<namespace>": <bytes>}}` when the source has a root-field namespace), subject to the queue cap
(§1.6); error → data/next message with `{"data":null,"errors":[...]}` as a control message.

Any hash function is acceptable for the Rust port (it is internal); use a 128/256-bit non-cryptographic or
cryptographic hash (e.g. xxh3-128 or BLAKE3) of the exact bytes.

### 6.7 Removal (`removeLiveQuery`)

Atomically: remove subscriber from its cohort (both maps), delete the cohort if empty, decrement the
operation-name count; if the poller has no cohorts left, remove it from the map and stop its task after the
transaction (`Immortal.stop`). A poll in flight may still call `onChange` for removed subscribers — the
operation-map check (§6.6) suppresses it.

### 6.8 Response format and ordering guarantees

- Each data message is the complete current result (not a diff).
- A subscriber's first message is always sent (even if identical to what the cohort last saw).
- Subsequent messages only when the cohort's DB bytes change. Because hashing is on bytes, a change in JSON
  key order or numeric formatting from Postgres would count as a change (not an issue in practice).
- Messages for one subscriber are delivered in poll order (single send queue per connection).
- Multiple subscriptions on one connection are independent; ordering between them is not guaranteed.

---

## 7. Streaming subscriptions

### 7.1 Schema (`GraphQL/Schema/SubscriptionStream.hs`, `Schema/Build.hs:195`)

For each table with a select permission (and stream fields not hidden by `hide_stream_fields`), subscription
root gets `<table>_stream` (custom name via table `custom_root_fields.select_stream`; naming convention
applies; source customization prefix/suffix/namespace apply):

```graphql
<table>_stream(
  batch_size: Int!                              # "maximum number of rows returned in a single batch"; non-negative
  cursor: [<table>_stream_cursor_input]!        # "cursor to stream the results returned by the query"
  where: <table>_bool_exp
): [<table>!]!

input <table>_stream_cursor_input {             # "Streaming cursor of the table \"<table>\""
  initial_value: <table>_stream_cursor_value_input!   # "Stream column input with initial value"
  ordering: cursor_ordering                     # "cursor ordering"; default ASC
}

input <table>_stream_cursor_value_input {       # "Initial value of the column from where the streaming should start"
  <col>: <col_type>                             # one optional field per selectable scalar column (column description)
}

enum cursor_ordering {                          # "ordering argument of a cursor"
  ASC                                           # "ascending ordering of the cursor"
  DESC                                          # "descending ordering of the cursor"
}
```

Under GraphQL-default naming the argument names follow the naming convention (`batchSize`, `initialValue`)
and type names are cased accordingly (see schema spec). `cursor_ordering` gets source type-name customization.

Argument validation (parse errors → `validation-failed` pre-exec errors):
- The flattened list of (cursor item × column specified in its `initial_value`) must contain **exactly one**
  entry. Zero → `one streaming column field is expected`; more than one (two list elements, or two columns in one
  `initial_value`) → `multiple column cursors are not supported yet`. `null` list elements are skipped.
- `batch_size` must be a non-negative Int.

### 7.2 SQL shape (`mkStreamSQLSelect`, `mkStreamingMultiplexedQuery`)

Root select for the stream field (cursor column `c` with GraphQL name `cn`, Postgres type `T`). Logical shape
before the identifier-renaming pass (verified against `Translate/Select/Streaming.hs`,
`Internal/GenerateSelect.hs` `defaultGenerateSQLSelect`/`generateSQLSelectFromArrayNode`, `Internal/OrderBy.hs`,
`Translate/Types.hs` `applySortingAndSlicing`/`orderByForJsonAgg`, `SQL/DML.hs` `ToSQL Select`/`ToSQL LimitExp`):

```sql
-- per cohort (inside the lateral); "root" is the select-source prefix
SELECT coalesce(json_agg("root" ORDER BY "root.pg.c" ASC), '[]') AS "root",       -- DESC for DESC
       json_build_object('<cn>', (MAX("root.pg.c"))::text) AS "cursor"            -- MIN(...) for DESC
FROM (
  -- node select: row JSON + one extractor per order-by column, alias "<prefix>.pg.<pgcol>"
  SELECT row_to_json((SELECT "_e" FROM (SELECT "root.base"."x" AS "x", ...) AS "_e")) AS "root",
         "root.base"."c" AS "root.pg.c"            -- redaction CASE applied here for inherited roles
  FROM (
    -- base select: ORDER BY and FETCH FIRST are applied HERE (cursor is a plain column => "sort at base")
    SELECT * FROM "schema"."table"
    WHERE <permission filter AND ("c" > ((_subs.result_vars #>> ARRAY['cursor','<cn>']))::T) AND <user where>>
                                                                                 -- '<' for DESC
    ORDER BY "c" ASC                                                             -- DESC for DESC
    FETCH FIRST <batch_size> ROWS WITH TIES                                      -- Postgres & Citus
  ) AS "root.base"
  ORDER BY "root.pg.c" ASC                                                       -- node order by, no limit
) AS "root"
```

Rendering facts (verified): `ToSQL Select` prints clauses in the order `SELECT [DISTINCT] extractors FROM WHERE
GROUP BY HAVING ORDER BY <limit> OFFSET`; `FetchFirstWithTiesExp n` renders `FETCH FIRST <n> ROWS WITH TIES`
(n printed as an integer literal, not a bind parameter); the plain variant renders `LIMIT <n>`. The cursor
comparison lives in the base `WHERE` together with the permission filter and the user's `where`
(`_saWhere = cursorBoolExp AND userWhere`). The `MAX`/`MIN` and the `json_agg` ordering reference the node-level
extractor alias `"root.pg.c"` (`contextualizeBaseTableColumn`), not `"root.base"."c"`. Exact whitespace /
renamed aliases: produce via the shared SQL printer + rename pass (SQL spec); confirm with
`/v1/graphql/explain` on a `_stream` subscription if byte-exactness is ever needed.

and the outer multiplexed query adds a third column:

```sql
SELECT "_subs"."result_id", "_fld_resp"."root" AS "result", "_fld_resp"."cursor" AS "cursor"
FROM UNNEST(($1)::uuid[], ($2)::json[]) AS "_subs"("result_id","result_vars")
LEFT OUTER JOIN LATERAL (
  SELECT json_build_object('<alias>', "<alias>"."root") AS "root",
         to_json("<alias>"."cursor") AS "cursor"
  FROM (<stream root select>) AS "<alias>"
) AS "_fld_resp" ON ('true')
```

Details:
- The comparison is strict (`>` ascending, `<` descending); the cursor key in `result_vars.cursor` is the
  column's **GraphQL field name** (custom column names respected — test
  `test_streaming_subscription_with_custom_name_set_for_cursor` uses `userId`).
- `batch_size` is a literal limit with `WITH TIES` (`enableWithTiesInSelectSource` sets `LimitArg True n`;
  `limitArgToLimitExp`, `GenerateSelect.hs:55`): Postgres/Citus emit `FETCH FIRST n ROWS WITH TIES` so rows
  sharing the last cursor value are all returned (a batch can exceed `batch_size`). Cockroach (and any other
  backend) emits a plain `LIMIT n` (rows can be skipped with duplicate cursor values — upstream acknowledges this
  as a known bug). `batch_size: 0` → `FETCH FIRST 0 ROWS WITH TIES` → always empty batches (nothing is ever sent).
- Column redaction expressions (inherited roles) apply to the cursor column ordering/comparison.
- The cursor max/min is computed over the rows selected in this batch and cast to `text`, so cursor values are
  text-encoded; it is `null` when no rows matched.
- Results are ordered by the cursor column; ties have no defined order.
- Validation at start (§6.4) additionally validates the initial cursor value (`validatedCursorVars`).

### 7.3 Cohorts and cursor advance (`Poll/StreamingQuery.hs`)

- Initial cohort key's `cursor` = `{"<cn>": "<initial value text>"}`. Subscribers with the same query shape,
  variables and *initial value* share a cohort.
- Each poll (same snapshot/batch/concurrency as §6.6, except new subscribers are *not* moved to existing at
  snapshot time) produces per cohort `(id, result bytes, cursor json)`.
- `newCursor = merge(prevCursor, rowCursor)` where a `null` value keeps the previous value.
- Push rule: if `isError || hash != previousHash`: store hash **and** store `newCursor` in the cohort's cursor
  ref; push to existing + new subscribers — **unless the result is the empty batch** `{"<alias>" : []}`
  (compared byte-wise to Postgres' rendering `{"<alias>" : []}`; Cockroach `\x01{"<alias>": []}`), in which case
  nothing is sent. Else (hash unchanged) push to new subscribers only (also suppressed when empty).
- After all batches, the **cohort map is rebuilt** atomically: every processed cohort is re-keyed with its
  updated cursor (so a later subscriber whose initial value equals an old cursor does not join an advanced
  cohort); subscribers that left during the poll are dropped; subscribers that joined during the poll stay in a
  cohort keyed by the old key so they are processed from their own initial value; cohorts that end up with equal
  keys are merged. Snapshotted new subscribers are moved to existing.
- Each push carries `resumeCursor` = the cursor in effect *before* the poll (used only in the over-capacity
  error, §1.6).

Observable behaviour (tests `TestStreamingSubscription`, `TestStreamingSubscriptionWithTies`,
`api-tests Test.Subscriptions.StreamingSubscriptionsSpec`):
- With 10 existing rows, `batch_size: 2`, `initial_value: {id: 0}` → five data messages, each
  `{"data":{"<alias>":[{id:1..},{id:2..}]}}`, `[3,4]`, ..., `[9,10]`, one per poll interval (≈1 s apart).
- Afterwards nothing is sent until new matching rows appear; then a message containing only the new rows.
- With duplicate cursor values (priorities `1,2,2,2,3,4,5`, batch 2): batches `[1,2,2,2]`, `[3,4]`, `[5]`.
- Streaming never re-sends updated rows (only rows past the cursor).

The cursor lives only in server memory per cohort; on reconnect clients must supply a new `initial_value`
themselves.

### 7.4 Error behaviour

Poll errors: same as live queries (error pushed to all cohort subscribers on every failing poll, cursor kept,
subscription stays active).

### 7.5 Removal (`removeStreamingQuery`)

Because cohort keys change, removal scans all cohorts of the poller for the subscriber id (reverse lookup)
rather than trusting the stored key; otherwise identical to §6.7.

### 7.6 `remove_empty_subscription_responses` experimental feature

When enabled, both live and streaming multiplexed SQL are wrapped (`removeEmptyMultiplexedResults`):

```sql
SELECT * FROM (<multiplexed select>) AS "_multiplex"
WHERE EXISTS (
  SELECT 1 FROM json_each("result")
  WHERE (json_typeof("value") = 'array' AND json_array_length("value") > 0)
     OR (json_typeof("value") <> 'array' AND json_typeof("value") <> 'null')
)
```

Rows whose every root field is `null` or `[]` are filtered in SQL. Observable for live queries: a subscriber
never receives an empty result (not even the initial one), and cannot observe a transition to empty. Changing
this flag is part of `SQLGenCtx` (not in the reaper's checks).

---

## 8. Error behaviour mid-subscription (summary)

| Event | Behaviour |
|---|---|
| Poll SQL error (DB down, function raises, permission SQL error) | Error pushed as `data`/`next` with `{"data":null,"errors":[{message, extensions:{path:"$", code}}]}` to all subscribers in the batch, on **every** poll while failing; subscription remains; next success always pushed. Error log `livequery-poller-log` with `errors: [{batch_id, detail}]`, level error. |
| Send queue full | operation cancelled with `error` + `complete` (§1.6). |
| Token expiry | socket dropped (§3.4). |
| Metadata change (same instance) | socket closed 1012. |
| Permission/metadata change on another instance | nothing; old plans keep running (§3.6). |
| Table dropped / column renamed via SQL | poll errors as above until clients resubscribe. |
| Stop / close | silent teardown. |

There is no automatic `complete` for live queries or streaming subscriptions; they run until stopped.

---

## 9. Async action subscriptions (`Execute.hs:118-124,272-281`, `Execute/Action/Subscription.hs`)

Subscription to an async action's query field `<action>(id: "<action_id>") { id created_at errors output { ... } }`
(see actions spec for the action log and response building).

Two kinds:

1. **No relationships on the output type** (`SEAsyncActionsWithNoRelationships`): all root fields must be async
   action fields (possibly several with `@_multiple_top_level_fields`). Registered in the **global**
   async-action map keyed by the client's operation id. A single background thread
   (`asyncActionSubscriptionsProcessor`, forked at startup, `App.hs:1415`) loops: for every registered op, fetch
   the action log rows (`hdb_catalog.hdb_action_log` by id) from metadata storage; on fetch error send a pre-exec
   style `error` and remove; otherwise **send a data message with the current response every iteration (no
   dedup)**; if all actions are complete (response or errors present), send `complete` and remove. Then sleep
   1 s. Result: clients get the same payload roughly every second until completion, then `complete`.
   Edge: this branch is also taken when the subscription has **no DB, streaming or action fields at all** (e.g.
   only `__typename` inside a source namespace). Then: with a modifier (namespaced introspection), a single
   data message with the modified empty object is sent and nothing else (no `complete`); without a modifier,
   just `complete` is sent (`WebSocket.hs`, `SEAsyncActionsWithNoRelationships` / `NE.nonEmpty` = `Nothing`).
   On a log-fetch error the processor calls `sendError` (an `error` message, **no `complete`**) and removes the
   op; if building the response fails (e.g. action id missing) an `error` is sent each iteration but the op is
   not removed.
   Quirks to **not** replicate blindly: the map is keyed by client operation id globally (two connections using
   id `"1"` collide), and these ops are not in the connection's operation map, so `stop` and socket close do not
   remove them (they are removed only on completion/error) — the Rust port should key by (connection, op id) and
   remove on stop/close.
2. **With table relationships** (`SSLivequery actionIds builder`): the action log is fetched, an SQL select
   embedding the action response (joined to the relationship tables) is built and run as a **normal multiplexed
   live query** (may mix with ordinary DB fields of the same source). The op is also registered in the async map;
   each processor iteration re-fetches the logs and, if they changed, **restarts** the live query
   (remove old subscriber, build & add new one with the new logs). When all actions complete it is removed from
   the async map (the live query keeps running and behaves as a normal live query). If restart fails, it is
   removed from the async map; an unexpected exception sends an error and stops the operation.

---

## 10. Multi-instance, Cloud Run, Cloudflare

### 10.1 Multi-instance behaviour

- All subscription state (pollers, cohorts, streaming cursors, connections) is **in-process memory**. Each
  instance polls the database independently for its own clients; there is no cross-instance coordination and no
  sticky sessions are required (a reconnecting client simply re-subscribes wherever it lands). Total DB load ≈
  Σ over instances of (pollers × batches) per interval.
- Async action subscriptions read the action log from the metadata DB, so the instance that executes the action
  webhook need not be the one serving the subscription.
- Schema/metadata changes propagate via schema sync (see metadata spec); only the instance serving the metadata
  API call closes its sockets (§3.6). The Rust engine should match, and may optionally (flag, default off) close
  sockets on schema-sync reloads too.
- Streaming cursors are per server-side cohort; on reconnect the client must resume with its own last seen cursor
  value as `initial_value`.

### 10.2 Google Cloud Run

- Cloud Run supports WebSockets over HTTP/1.1 upgrade with no extra config; each open socket counts as one
  in-flight request against the container's `concurrency` (max 1000) and is cut at the service **request
  timeout** (default 300 s, max 3600 s). Clients reconnect automatically (Apollo/graphql-ws do), so set the
  timeout to 3600 s and rely on reconnect. Keep-alive (5 s) keeps intermediaries from idling the connection.
- Use HTTP/1.1 (not end-to-end HTTP/2) for WebSocket upgrade (RFC 8441 websockets-over-h2 not supported there
  **(inferred)**).
- CPU: with request-based billing CPU is allocated while any request (including an open WebSocket) is active,
  so pollers run as long as a client is connected; pollers only exist while subscribers exist, so this is
  sufficient. Instance-based billing ("CPU always allocated") is still recommended for event triggers / cron
  (other specs). Session affinity is optional (not needed).
- On SIGTERM (scale-in / deploy) implement the graceful shutdown of §3.6: reject new upgrades with 503, close
  sockets with 1012 so clients reconnect to another instance.
- Size the DB pool for (#pollers × #batches) concurrent poll queries + HTTP traffic; batches of one poller run
  concurrently.

### 10.3 Cloudflare / serverless

Live queries require a long-lived process that polls Postgres on a timer and holds sockets. On Cloudflare
Workers this is only feasible with Durable Objects (WebSocket hibernation + alarms as the poll timer, Postgres via
Hyperdrive/TCP sockets) and would be a separate runtime adapter. Recommendation: treat WebSocket subscriptions as
unsupported on edge/serverless targets in v1 (HTTP queries/mutations work), keep the subscription engine behind a
trait (`SubscriptionTransport`, `Poller` scheduler) so a Durable-Object adapter can be added later. Hasura V2 has
no HTTP fallback (no SSE / graphql-sse); adding one is an optional extension, not a compatibility requirement.

---

## 11. Logging and introspection endpoints

### 11.1 `websocket-log` (engine log type `websocket-log`, `WebSocket.hs:159-208`)

```json
{"type":"websocket-log","level":"info","detail":{
  "user_vars": {"x-hasura-role":"user","x-hasura-user-id":"1"},      // null before init
  "connection_info": {"websocket_id":"<uuid>","token_expiry":"<utc>|null","msg":null},
  "event": {"type":"operation","detail":{
      "operation_id":"1","request_id":"<id>","operation_name":"Foo",
      "operation_type":{"type":"started"},                        // started|completed|stopped|proto_err{detail:text}|query_err{detail:QErr}
      "query": {"query":"...","variables":{...},"operationName":"Foo"},   // only if query-log enabled; variables removed unless analyze_query_variables
      "parameterized_query_hash":"<hex>"}}}}
```

Event types: `accepted`, `rejected` (detail QErr), `conn_err` (detail text), `operation`, `closed`. Level is
`error` for `rejected`, `conn_err`, `proto_err`, `query_err`; else `info`.

### 11.2 `ws-server` internal log (debug, `Server.hs:115-176`)

`{"websocket_id": "...", "event": {"type": "connection_request"|"connection_timeout"|"accepted"|"rejected"|"message_received"|"message_sent"|"jwt_expired"|"close_received"|"close_sent"|"closed", "detail": ...}, "metadata": {"event_type","operation_id","operation_name","query_execution_time","response_size","parameterized_query_hash","streaming_resume_cursor"}}`.
Received message bodies are logged as `"<censored>"` unless metrics config `analyze_query_variables` is on.

### 11.3 `livequery-poller-log` (`Poll/Common.hs` `pollDetailMinimal`)

```json
{"poller_id":"<uuid>","kind":"live-query"|"streaming","snapshot_time":0.00001,
 "batches":[...],"execution_batches":[{"db_execution_time":0.002,"pg_execution_time":0.002,"push_time":0.0001,"batch_id":1,"batch_response_size_bytes":123}],
 "subscriber_count":<number of cohorts>,"total_time":0.003,"source":"default","generated_sql":"...",
 "role":"user","subscription_options":{"batch_size":100,"refetch_delay":1},
 "errors":[{"batch_id":1,"detail":<QErr>}]}            // only when errors; level error
```

Emitted after **every** poll at level info (this log type is not enabled by default — see logging spec).
`kind` values: `"live-query"` / `"streaming"` (`RQL/Types/Subscription.hs`).

### 11.4 `/v1/graphql/explain` for subscriptions

Response `{"sql": "<multiplexed SQL text>", "plan": ["EXPLAIN lines"...], "variables": <CohortVariables JSON>}`
(`SubscriptionQueryPlanExplanation`); `EXPLAIN` (never `ANALYZE`) is run with a fresh cohort id. Admin only,
`user` field supplies session variables. See SQL/explain spec.

### 11.5 `/dev/subscriptions`, `/dev/subscriptions/extended` (dev mode only)

`{"options": LiveQueriesOptions, "live_queries_map": [...], "stream_queries_map": [...], "stream_queries_options": ...}`;
each poller `{"source","role","thread_id","poller_id","multiplexed_query","cohorts"(extended only: [{variables, cohort:{resp_id,current_ops,new_ops,previous_result_hash}}]),"parameterized_query_hash"}`.
"Output subject to change" — low priority.

### 11.6 Metrics (EE)

EKG gauges `websocket_connections`, `active_subscriptions`, `active_live_queries`, `active_streaming_subscriptions`
and Prometheus metrics (active pollers, pollers in error, subscription DB/total time histograms, websocket
bytes/messages queued/dropped, queue time) are EE/Cloud features; CE needs none, but the Rust engine should expose
equivalents on its own metrics endpoint for the performance comparison work.

---

## 12. Rust implementation notes (design guidance)

- **Transport**: `axum` (or hyper) + `tokio-tungstenite`. One task for reading, one writer task fed by a
  bounded data queue (capacity = `WEBSOCKET_QUEUE_SIZE`, `try_send`; on full → cancellation of §1.6) plus an
  unbounded control channel and a `watch`-style keep-alive slot; writer prefers control/data FIFO order as one
  queue (keep a single ordered queue with a separate counter for data messages to preserve exact ordering
  between control and data messages, as Hasura does).
- **Message handling**: process client messages in order. Hasura executes queries/mutations inline (blocking
  the connection); the Rust engine may spawn query/mutation execution concurrently (benign improvement) but must
  keep `start`/`stop` of the same id ordered (register the subscription synchronously before handling the next
  message).
- **Protocol abstraction**: a `Protocol` trait with two impls (`Apollo`, `GraphqlTransportWs`) providing:
  data message type, error encoding (`{"errors":[...]}` vs `[...]`), post-exec error representation, keep-alive
  message, init-failure action, start-error action, parse-failure action, init-timeout close code. This mirrors
  `WSActions` (`WSServerApp.hs:mkWSActions`).
- **Subscription engine** (backend-agnostic core, Postgres impl): `PollerRegistry` = `DashMap<PollerKey,
  Arc<Poller>>`; `Poller` holds `Mutex<IndexMap<CohortKey, Cohort>>`; one tokio task per poller with
  `loop { poll().await; sleep(interval).await }`; cancellation token on last unsubscribe. Batches via
  `futures::join_all` on pooled connections. Pre-serialise each cohort's variables JSON once.
- **SQL**: build with the same SQL AST/printer as queries; prepare once per poller (statement cache); bind
  `$1 uuid[]` and `$2 json[]` (text-encoded JSON array is fine for Postgres; jsonb binary only needed for
  Cockroach).
- **Change detection**: store a 128-bit hash per cohort; compare raw bytes' hash. Avoid parsing result JSON;
  splice bytes directly into the outgoing message (`{"type":"data","id":...,"payload":{"data":` + bytes + `}}`),
  JSON-escaping only the id.
- **Optional (off by default) optimisation**: LISTEN/NOTIFY-triggered early polls; must not change observable
  semantics (still dedup by hash, still at most one poll per poller in flight).

---

## Open questions / uncertainties

1. ~~Exact JSON key of the close-reason object~~ — **resolved (review)**: `{"server_error_msg": "..."}`, from
   `hasuraJSON = aesonPrefix snakeCase` applied to field `unServerErrorMsg` (§2.4). No test checks it.
2. ~~JWT/webhook expiry close frame~~ — **resolved (review)**: the Hasura code path makes no close call on expiry
   (§3.4); only the final "stream closed without close frame" step is library behaviour (inferred).
3. ~~Rendering of `WITH TIES` / stream root select~~ — **resolved (review)**: `ORDER BY "c"` + `FETCH FIRST n ROWS
   WITH TIES` sit in the base select; node select re-orders by `"root.pg.c"`; `json_agg` and `MAX/MIN` use
   `"root.pg.c"` (§7.2). Byte-exact whitespace not checked against a running engine.
4. Whether `requestPath` includes the query string (affects `/v1/graphql?x` acceptance) — still inferred
   (depends on `wai-websockets` building `requestPath` from raw path + raw query string).
5. Order of `error`(queued) vs direct `error` + close on graphql-ws start errors is racy in V2; any order is
   acceptable.
6. ~~Header precedence~~ — **resolved (review)**: not uniform; see the table in §3.1 (session vars incl. admin
   secret: upgrade header wins; JWT-mode role: payload wins; duplicate JWT tokens: error).

## Upstream tests to reuse

Python (server/tests-py; run with a websocket client):

- `test_subscriptions.py::TestSubscriptionCtrl`, `TestSubscriptionCtrlWithoutSecret` — `connection_init`/ack and `connection_terminate` (no further events).
- `test_subscriptions.py::TestSubscriptionBasicNoAuth` — init without credentials closes the socket (both protocols).
- `test_subscriptions.py::TestSubscriptionBasic` (Apollo) and `TestSubscriptionBasicGraphQLWS` — `connection_error` on bad message type, start/data, stop without id, stop, query over WS → data + complete; `test_negative` runs `queries/subscriptions/basic/negative_test.yaml` over http, websocket and subscription transports. Skip mssql parametrisation.
- `test_subscriptions.py::TestSubscriptionLiveQueries`, `TestSubscriptionLiveQueriesForGraphQLWS` + `queries/subscriptions/live_queries/{setup,steps,teardown}.yaml` — live updates after mutations (mutations sent over WS too), limits via variables, multiple subscriptions on one socket.
- `test_subscriptions.py::TestStreamingSubscription` + `queries/subscriptions/streaming/` — batching, concurrent inserts, custom cursor column name.
- `test_subscriptions.py::TestStreamingSubscriptionWithTies` — `WITH TIES` batches (Postgres/Citus only).
- `test_subscriptions.py::TestSubscriptionMultiplexingPostgresMSSQL`, `TestSubscriptionMultiplexingPostgres` + `queries/subscriptions/multiplexing/` — cohort variables via `/v1/graphql/explain` (session filtering, synthetic variables). Run Postgres variant only.
- `test_subscriptions.py::TestSubscriptionUDFWithSessionArg` + `queries/subscriptions/udf_session_args/` — function with whole-session argument (`UVSession`).
- `test_subscriptions.py::TestSubscriptionCustomizedSourceMSSQLPostgres` + `queries/subscriptions/customized_source/` — namespaced source wrapping, `@_multiple_top_level_fields` double-alias error message. Postgres variant only.
- `test_subscriptions.py::TestSubscriptionCohortIsolationMSSQLPostgres` + `queries/subscriptions/cohort_isolation/` — cohorts in the same poller never receive each other's rows (Postgres must pass).
- `test_subscriptions.py::TestSubscriptionMSSQLChunkedResults` — MSSQL only, skip.
- `test_graphql_queries.py::TestGraphQLExplainPostgresMSSQL::test_documented_subscription` + `queries/explain/docs_subscription.yaml` — byte-exact multiplexed SQL text (Postgres file; skip `_mssql`).
- `queries/endpoints/endpoint_subscription.yaml` (via `test_endpoints.py`) — REST endpoints must reject subscriptions (cross-check with REST spec).
- `validate.py` `transport='websocket'`/`'subscription'` mode: every `check_query_f(..., transport)` parametrised test in `test_graphql_queries.py`, `test_jwt.py`, `test_webhook.py`, `test_remote_relationships.py`, etc. re-runs HTTP fixtures over WS (queries as `start`, and as subscriptions with `@_multiple_top_level_fields`); these give broad coverage of query execution over the socket and of the error shapes (graphql-ws errors wrapped as `{"errors": payload}` by the harness).
- `test_jwt.py` / `test_webhook.py` websocket cases — auth via `connection_init` headers and expiry (check the specific `ws` tests in those files; webhook `Expires` handling).
- `test_dev_endpoints.py` — `/dev/subscriptions` availability in dev mode (low priority).

Haskell api-tests (server/lib/api-tests/src/Test; harness uses the Apollo protocol, `Harness/Subscriptions.hs`):

- `Subscriptions/RowUpdatesSpec.hs` — initial empty result, updates after insert, multiplexing of two cohorts, live query with variables.
- `Subscriptions/DerivedDataSpec.hs` — subscriptions over computed/derived data (functions, aggregates).
- `Subscriptions/StreamingSubscriptionsSpec.hs` — in-order streaming of new rows.
- `Regression/MultiplexQuerySpec.hs` — regression #9281: multiplexed query with computed field + function root field (validation query must surface runtime errors).
- `Regression/StreamConflictSpec.hs` — `_stream` root field naming does not conflict with a table named `*_stream`.
- `Databases/Postgres/DataValidation/RemoteSourcePermissionPredicateSpec.hs` — "Subscriptions on fields with permission based on remote relationship shouldn't be allowed" → `subscriptions on this field is not supported`.
- `Auth/Authorization/DisableRootFields/**` — `subscription_root_fields` in select permissions (which subscription fields exist).
- `Auth/Authorization/InheritedRoles/ColumnRedactionSpec.hs` — redaction applies to subscriptions/stream cursor.
- `Queries/Errors/EmptyRootsAreNotAvailableSpec.hs` — `no subscriptions exist` style errors when roots are empty.
- `Schema/CustomFieldsSpec.hs`, `PortedFromPytest/TestGraphQLQueryFunctions.hs` — custom root field names incl. `select_stream`, function roots in subscriptions.
- Skip: MSSQL/BigQuery/Data Connector backends of the above fixtures.
