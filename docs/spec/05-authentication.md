# 05 - Authentication

Compatibility spec for how Hasura GraphQL Engine V2 turns an incoming HTTP or WebSocket request into a `UserInfo`: a role, session variables and the backend-only flag. It also covers CORS. Ground truth is the Haskell source under `server/`. Docs live under `docs/docs/auth/authentication/`. Where they disagree, the source wins, and this spec says so. Statements marked "(inferred)" were not verified line by line.

What the role can do once resolved is covered in `04-permissions.md`.

Primary source files, relative to the repo root:

- `server/src-lib/Hasura/Server/Auth.hs`: `AuthMode`, `setupAuthMode`, `getUserInfoWithExpTime`, JWK refresh.
- `server/src-lib/Hasura/Server/Auth/JWT.hs`: JWT config, verification and claims.
- `server/src-lib/Hasura/Server/Auth/WebHook.hs`: webhook mode.
- `server/src-lib/Hasura/Authentication/{Session,User,Role,Headers}.hs`: session variables, `mkUserInfo`, header lists.
- `server/src-lib/Hasura/Server/Init.hs`, `server/src-lib/Hasura/Server/Init/Arg/Command/Serve.hs`, `server/src-lib/Hasura/Server/Init/Env.hs`: flags and env vars.
- `server/src-lib/Hasura/Server/Cors.hs`, `server/src-lib/Hasura/Server/Middleware.hs`: CORS.
- `server/src-lib/Hasura/Server/App.hs`: HTTP pipeline, status mapping, `onlyAdmin`.
- `server/src-lib/Hasura/App.hs`: `authorize*Api`, `updateJwkCtxThread`.
- `server/src-lib/Hasura/GraphQL/Transport/WebSocket.hs`, `.../WebSocket/Server.hs`, `.../WebSocket/Protocol.hs`, `.../WSServerApp.hs`: WebSocket auth.
- `server/lib/hasura-base/src/Hasura/Base/Error.hs`: error codes.

---

## Scope and priority

| Feature | Priority | Edition |
|---|---|---|
| No-auth mode (everything is admin unless `x-hasura-role` is sent) | Essential | OSS |
| Admin secret (`HASURA_GRAPHQL_ADMIN_SECRET`, header `x-hasura-admin-secret`) | Essential | OSS |
| Admin secret + `x-hasura-role` impersonation | Essential | OSS |
| Unauthorized role (`HASURA_GRAPHQL_UNAUTHORIZED_ROLE`) | Essential | OSS |
| JWT mode: HS*/RS* keys, `claims_namespace`, `claims_format`, `audience`, `issuer`, `allowed_skew`, `header` | Essential | OSS |
| JWT `jwk_url` with Cache-Control/Expires refresh | Essential | OSS |
| JWT `claims_namespace_path`, `claims_map` | Common | OSS |
| JWT ES*/Ed25519 keys, `extra_required_claims`, custom header | Rare | OSS |
| Webhook mode GET/POST, `send_request_body` | Essential | OSS |
| WebSocket `connection_init` auth and token expiry | Essential | OSS |
| CORS (`HASURA_GRAPHQL_CORS_DOMAIN`, `--disable-cors`, `--ws-read-cookie`) | Essential | OSS |
| `HASURA_GRAPHQL_PRESERVE_401_ERRORS` | Rare | OSS (recent) |
| `HASURA_GRAPHQL_DISABLE_ADMIN_SECRET` | Rare | OSS |
| Multiple admin secrets (`HASURA_GRAPHQL_ADMIN_SECRETS`) | Skip first | EE/Cloud only; the OSS parser reads a single secret |
| Multiple JWT secrets (`HASURA_GRAPHQL_JWT_SECRETS`) | Skip first | EE/Cloud only; OSS reads one `HASURA_GRAPHQL_JWT_SECRET` (the internal types support lists) |
| Webhook auth response caching (Redis) | Skip | EE/Cloud only (docs `webhook.mdx` "Webhook Auth Caching") |
| SSO / admin-secret-less console login, API limits, auth via Hasura Cloud | Skip | EE/Cloud |

Even for OSS, implement the internal model as a set of admin secret hashes and a list of JWT contexts. Upstream does this, and it makes the EE extensions trivial.

---

## 1. Concepts

### 1.1 Session variables

- A session variable is any request header (or webhook/JWT claim key) whose name starts with `x-hasura-`, compared case-insensitively. See `isSessionVariable` in `Authentication/Session.hs`.
- Names are case-insensitive. In JSON output and SQL they appear lowercased: `x-hasura-user-id`.
- Values are always text. Building them from a list of headers is a map insert, so for duplicate headers (names compared case-insensitively) the last one wins (`mkSessionVariablesHeaders` uses `HashMap.fromList`; verified).
- The JSON form, used in action payloads, the `hasura.user` setting and webhooks, is an object with lowercase keys and string values.

### 1.2 Well-known names

| Name | Meaning |
|---|---|
| `x-hasura-role` | requested role |
| `x-hasura-admin-secret` | admin secret (never kept in session) |
| `x-hasura-allowed-roles` | JWT claim, list of roles |
| `x-hasura-default-role` | JWT claim, default role |
| `x-hasura-use-backend-only-permissions` | backend-only flag (see 04 section 8) |
| `x-hasura-access-key` | legacy header, NO LONGER ACCEPTED; treated as "no secret sent" (`server/src-test/Hasura/Server/AuthSpec.hs`) |

### 1.3 UserInfo

`mkUserInfo` (`Authentication/User.hs`) takes a role source, an admin-secret state and session variables.

Role source:

- `URBFromSessionVariables`: role = `x-hasura-role` from the given session variables. If absent: 400 `invalid-params`, message `"x-hasura-role" not found in session variables`.
- `URBFromSessionVariablesFallback admin`: role = `x-hasura-role` if present, else `admin`.
- `URBPreDetermined r`: role = r.

Other rules:

- A role name must be non-empty. An empty `x-hasura-role` value counts as absent.
- The final session variables are the given ones minus `x-hasura-admin-secret`, plus `x-hasura-role` set to the resolved role.

Backend-only flag:

- Computed only when the admin-secret state is `UAdminSecretSent` or `UAuthNotSet` (no admin secret configured).
- Otherwise it is always disallowed.
- With the header `x-hasura-use-backend-only-permissions`:
  - `true`/`t`/`yes`/`y` gives allowed.
  - `false`/`f`/`no`/`n` gives disallowed.
  - Matching is case-insensitive.
  - Anything else gives 400 `bad-request`:

    ```
    "x-hasura-use-backend-only-permissions":  Not a valid boolean text. True values are ["true","t","yes","y"] and  False values are ["false","f","no","n"]. All values are case insensitive
    ```

---

## 2. Configuration

| Env var | Flag | Default | Notes |
|---|---|---|---|
| `HASURA_GRAPHQL_ADMIN_SECRET` | `--admin-secret` | unset | Stored as a SHA-512 hash (`hashAdminSecret`). |
| `HASURA_GRAPHQL_ACCESS_KEY` | `--access-key` | unset | Deprecated alias. Used only if ADMIN_SECRET is unset (`withOptions [adminSecretOption, accessKeyOption]`). |
| `HASURA_GRAPHQL_DISABLE_ADMIN_SECRET` | `--disable-admin-secret` | `false` | Disables admin-secret authentication; see the mode table. |
| `HASURA_GRAPHQL_AUTH_HOOK` | `--auth-hook` | unset | Webhook URL. |
| `HASURA_GRAPHQL_AUTH_HOOK_MODE` | `--auth-hook-mode` | `GET` | Exactly `GET` or `POST` (case-sensitive). Error: `Only expecting GET / POST`. Legacy env `HASURA_GRAPHQL_AUTH_HOOK_TYPE` is also read. |
| `HASURA_GRAPHQL_AUTH_HOOK_SEND_REQUEST_BODY` | `--auth-hook-send-request-body` | `true` | Forced to `false` in GET mode. |
| `HASURA_GRAPHQL_JWT_SECRET` | `--jwt-secret` | unset | JSON object (section 5). |
| `HASURA_GRAPHQL_UNAUTHORIZED_ROLE` | `--unauthorized-role` | unset | Role for unauthenticated requests. |
| `HASURA_GRAPHQL_CORS_DOMAIN` | `--cors-domain` | `*` | Section 8. |
| `HASURA_GRAPHQL_DISABLE_CORS` | `--disable-cors` | `false` | |
| `HASURA_GRAPHQL_WS_READ_COOKIE` | `--ws-read-cookie` | `false` | Only allowed when CORS is disabled. |
| `HASURA_GRAPHQL_PRESERVE_401_ERRORS` | `--preserve-401-errors` | `false` | Section 9.2. |
| `HASURA_GRAPHQL_ADMIN_INTERNAL_ERRORS` | `--admin-internal-errors` | `true` | Include `extensions.internal` for admin. |
| `HASURA_GRAPHQL_CONFIGURED_HEADER_PRECEDENCE` | | client headers first | For actions and validate_input header merging (not auth). |
| `HASURA_GRAPHQL_ADMIN_SECRETS` | | | EE/Cloud only: JSON list of secrets. Ignored by OSS. |
| `HASURA_GRAPHQL_JWT_SECRETS` | | | EE/Cloud only: JSON list of JWT configs. Ignored by OSS. |

When neither flag nor env is given, the default applies. Flags override env vars.

---

## 3. Auth mode selection (startup)

`setupAuthMode` (`Server/Auth.hs`) matches on (admin secret set?, webhook set?, JWT set?, disable-admin-secret?):

| Admin secret | Webhook | JWT | Disable admin secret | Result |
|---|---|---|---|---|
| any | no | no | yes | `AMUnauthentication`: every request gets 401 `unauthenticated` |
| no | no | no | no | `AMNoAuth`. If an unauthorized role is set: fatal `Fatal Error: --unauthorized-role (HASURA_GRAPHQL_UNAUTHORIZED_ROLE) requires --admin-secret (HASURA_GRAPHQL_ADMIN_SECRET) to be set` |
| yes | no | no | no | `AMAdminSecret(secrets, unauthRole?)` |
| any | no | yes | yes | `AMJWT(jwtCtxs, unauthRole?)` (admin secret is NOT checked) |
| yes | no | yes | no | `AMAdminSecretAndJWT(secrets, jwtCtxs, unauthRole?)` |
| no | no | yes | no | fatal `Fatal Error : --jwt-secret (HASURA_GRAPHQL_JWT_SECRET) requires --admin-secret (HASURA_GRAPHQL_ADMIN_SECRET) to be set` |
| any | yes | no | yes | `AMHook(hook)` |
| yes | yes | no | no | `AMAdminSecretAndHook`. If an unauthorized role is set: fatal `Fatal Error: --unauthorized-role (HASURA_GRAPHQL_UNAUTHORIZED_ROLE) is not allowed when --auth-hook (HASURA_GRAPHQL_AUTH_HOOK) is set` |
| no | yes | no | no | fatal `Fatal Error : --auth-hook (HASURA_GRAPHQL_AUTH_HOOK) requires --admin-secret (HASURA_GRAPHQL_ADMIN_SECRET) to be set` |
| any | yes | yes | any | fatal `Fatal Error: Both webhook and JWT mode cannot be enabled at the same time` |

Notes:

- Fatal errors abort startup. `Fatal Error :` (space before the colon) vs `Fatal Error:` is as in the source.
- Duplicate JWT configs are removed (`nub`) before building contexts.
- In `AMHook` and `AMJWT` (disable-admin-secret), an `x-hasura-admin-secret` header is silently ignored; it is stripped from the session.
- `/v1alpha1/config` (admin only) reports `is_admin_secret_set` (true for every mode except `AMNoAuth`), `is_auth_hook_set`, `is_jwt_set` and `jwt` (see the HTTP spec).

---

## 4. Per-request resolution

The algorithm is `getUserInfoWithExpTime_`. Input: the raw request headers, the auth mode, and (for webhook POST) the parsed GraphQL request(s). Output: `(UserInfo, Maybe expiryTime, responseHeadersToAdd)`. Here `sessionVars(headers)` = all `x-hasura-*` request headers.

```
case mode of
  AMNoAuth ->
      mkUserInfo(FallbackAdmin, UAuthNotSet, sessionVars(headers))
      -- role = x-hasura-role header or "admin"; backend-only header honoured
  AMUnauthentication ->
      401 access-denied "unauthenticated"
  AMAdminSecret(secrets, unauthRole) ->
      checkingSecretIfSent(secrets,
        if unauthRole is None:
            401 access-denied "\"x-hasura-admin-secret\" required, but not found"
        else:
            mkUserInfo(PreDetermined unauthRole, UAdminSecretNotSent, sessionVars(headers)))
  AMHook(hook)                         -> webhook(hook)
  AMAdminSecretAndHook(secrets, hook)  -> checkingSecretIfSent(secrets, webhook(hook))
  AMJWT(ctxs, unauthRole)              -> jwt(ctxs, unauthRole)
  AMAdminSecretAndJWT(secrets, ctxs, unauthRole)
                                       -> checkingSecretIfSent(secrets, jwt(ctxs, unauthRole))

checkingSecretIfSent(secrets, otherwise):
  match header x-hasura-admin-secret (case-insensitive name; last wins):
    absent  -> otherwise
    present ->
      if sha512(value) not in secrets:
          401 access-denied "invalid \"x-hasura-admin-secret\""
      else:
          mkUserInfo(FallbackAdmin, UAdminSecretSent, sessionVars(headers))
          -- impersonation: role = x-hasura-role header or admin;
          -- all x-hasura-* headers become session variables;
          -- backend-only header honoured
```

Key behaviours:

- **Admin secret.** A correct admin secret always wins over JWT and webhook. The JWT or webhook is not consulted at all.
- **Wrong secret.** A wrong admin secret (including the empty string) is a 401 even if a valid JWT is also present.
- **Unauthorized role.** With an unauthorized role, the request still carries any client-sent `x-hasura-*` headers as session variables. Upstream behaviour: the client can set them. The `x-hasura-role` header is ignored; the role is forced to the unauthorized role.
- **Admin role.** Role `admin` reached by any path (no-auth default, impersonation, JWT allowed role `admin`, webhook returning `admin`) has full admin rights.

### 4.1 Status codes and error codes

| Situation | Real status | code | message |
|---|---|---|---|
| Unauthentication mode | 401 | `access-denied` | `unauthenticated` |
| Admin secret required | 401 | `access-denied` | `"x-hasura-admin-secret" required, but not found` |
| Admin secret wrong | 401 | `access-denied` | `invalid "x-hasura-admin-secret"` |
| Webhook says 401 | 401 | `access-denied` | `Authentication hook unauthorized this request` |
| Webhook response not decodable | 500 | `unexpected` | `Invalid response from authorization hook: <aeson error>` |
| Webhook other status | 500 | `unexpected` | `Invalid response from authorization hook` |
| Webhook HTTP failure | 500 | `unexpected` | `webhook authentication request failed` |
| Webhook 200 without X-Hasura-Role | 400 | `invalid-params` | `"x-hasura-role" not found in session variables` |
| JWT errors | 400 | various | see section 5.8 |
| Bad backend-only header | 400 | `bad-request` | see 1.3 |

`throw401` always uses code `access-denied`.

Error body format:

- `/v1/graphql`, `/v1alpha1/graphql`, relay, GraphQL GET:

  ```json
  {"errors":[{"extensions":{"path":"$","code":"access-denied"},"message":"invalid \"x-hasura-admin-secret\""}]}
  ```

- `/v1/metadata`, `/v1/query`, `/v2/query` and other REST-like endpoints:

  ```json
  {"path":"$","error":"invalid \"x-hasura-admin-secret\"","code":"access-denied"}
  ```

  The field order is not significant. An `internal` key is added only when internal errors are enabled for the role.

HTTP status for GraphQL endpoints is covered in section 9.2.

---

## 5. JWT mode

Docs: `docs/docs/auth/authentication/jwt.mdx`. Code: `Server/Auth/JWT.hs`.

### 5.1 Config JSON (`HASURA_GRAPHQL_JWT_SECRET`)

```json
{
  "type": "RS256",
  "key": "-----BEGIN PUBLIC KEY-----\n...\n-----END PUBLIC KEY-----",
  "jwk_url": "https://example.com/.well-known/jwks.json",
  "claims_namespace": "https://hasura.io/jwt/claims",
  "claims_namespace_path": "$.hasura.claims",
  "claims_format": "json",
  "claims_map": { ... },
  "audience": "myapp",
  "issuer": "https://issuer.example.com/",
  "allowed_skew": 60,
  "header": {"type": "Authorization"},
  "extra_required_claims": ["audience", "issuer"]
}
```

| Field | Type | Default | Rules |
|---|---|---|---|
| `type` | string | n/a | Required with `key`. One of `HS256`, `HS384`, `HS512`, `RS256`, `RS384`, `RS512`, `Ed25519`, `ES256`, `ES384`, `ES512` (case-sensitive). |
| `key` | string | n/a | HS*: the raw shared secret (UTF-8 bytes used as-is). Must be at least 32/48/64 bytes for HS256/384/512, else `Invalid JWK: Key size too small; should be atleast 32 characters` (resp. 48, 64) (`parseHmacKey`, `Server/Auth/JWT/Internal.hs`). RS*: PEM public key or X.509 certificate (`Invalid JWK: Could not decode PEM: <e>` on failure). ES*/Ed25519: PEM. |
| `jwk_url` | URL string | n/a | Mutually exclusive with `key`. |
| `claims_namespace` | string | `https://hasura.io/jwt/claims` | Key of the Hasura claims object among the token's claims. |
| `claims_namespace_path` | JSONPath string | n/a | Mutually exclusive with `claims_namespace`. Evaluated against the token's non-registered claims. |
| `claims_format` | `json` or `stringified_json` | `json` | |
| `claims_map` | object | n/a | When present, `claims_namespace*` and `claims_format` are ignored. |
| `audience` | string or [string] | none | |
| `issuer` | string | none | |
| `allowed_skew` | number (seconds) | 0 | Leeway for `exp` and `nbf`. |
| `header` | object | `{"type":"Authorization"}` | `{"type":"Cookie","name":"<cookie>"}` or `{"type":"CustomHeader","name":"<header>"}`. `type` is case-insensitive. |
| `extra_required_claims` | [`audience` / `issuer`] | `[]` | Makes a missing `aud` / `iss` claim a failure. |

Config parse errors (fatal at startup; message wrapped by the env parser):

- `key and jwk_url both cannot be empty`
- `key, jwk_url both cannot be present`
- `claims_namespace and claims_namespace_path both cannot be set`
- `invalid JSON path claims_namespace_path error: <err>`
- `Invalid JWK: Key type: <type> is not supported`
- `Invalid JWK: <key parse error>`
- `expected 'type' is 'Authorization' or 'Cookie' or 'CustomHeader'`
- `Unknown extra_required_claims value: <v>`
- claims_map: `x-hasura-allowed-roles is expected but not found`, `x-hasura-default-role is expected but not found`

Serialised form (for `/v1alpha1/config`): `type` and `key` are redacted to `"<TYPE REDACTED>"` and `"<JWK REDACTED>"`.

### 5.2 Token location

- `Authorization` (default):
  - Header name is case-insensitive.
  - The value is split on whitespace and must be exactly the two words `Bearer` and `<token>`. `Bearer` is case-sensitive.
  - Otherwise 400 `invalid-headers`, `Malformed Authorization header`. This also hits `Basic ...` headers in JWT mode.
- `Cookie`: cookies are parsed from the `Cookie` header and the named cookie's value is the token.
- `CustomHeader`: the named header (matched case-insensitively) holds the raw token with no `Bearer` prefix.

### 5.3 Selecting the JWT config (multiple configs)

OSS has a single config, but the algorithm is general:

1. Collect `(location, token)` pairs from the headers. Keep only those whose location matches some config's `header`.
2. For each (config, token) pair, decode the token's `iss` without verification:
   - Config has no `issuer`: match.
   - Token has no `iss`: match, unless `extra_required_claims` contains `issuer`, in which case mismatch.
   - Otherwise, match if equal.
3. Decide:
   - No pairs at all: the "no token" path.
     - If an unauthorized role is configured, use it, with session vars from the `x-hasura-*` request headers and no expiry.
     - Else 400 `invalid-headers`, `Missing 'Authorization' or 'Cookie' header in JWT authentication mode`.
   - Only mismatches: 400 `invalid-jwt`, `Could not verify JWT: JWTNotInIssuer`.
   - Exactly one match: verify with that config.
   - More than one match: 400 `invalid-headers`, `Could not verify JWT: Multiple JWTs found`.

Consequence: an invalid or expired token never falls back to the unauthorized role. Only a missing token does.

### 5.4 Verification

The jose `verifyClaimsAt` uses the current time and the JWK set:

- Signature against any key in the set (the `kid` is used by jose for selection).
- `exp` and `nbf` with `allowed_skew` leeway.
- `iss` must equal `issuer` when configured and present in the token.
- `aud`: checked only if the token has `aud`. Any of the token audiences must be in the configured audience list. With `extra_required_claims: ["audience"]`, a missing `aud` fails with `JWTNotInAudience`.

Failure: 400 `invalid-jwt`, message `Could not verify JWT: <show JWTError>`. The exact strings from the Haskell `show` of jose errors:

| Cause | message |
|---|---|
| expired | `Could not verify JWT: JWTExpired` |
| not yet valid | `Could not verify JWT: JWTNotYetValid` |
| bad audience | `Could not verify JWT: JWTNotInAudience` |
| bad issuer | `Could not verify JWT: JWTNotInIssuer` |
| bad signature | `Could not verify JWT: JWSError JWSInvalidSignature` |
| no key matched | `Could not verify JWT: JWSError NoUsableKeys` |
| malformed | `Could not verify JWT: JWSError (CompactDecodeError ...)` (Haskell show; reproduce the prefix, the tail is best-effort) |

The token `exp` (if present) becomes the request's expiry time, used by WebSockets.

### 5.5 Claims extraction

Namespace mode (no `claims_map`):

1. Find the claims value:
   - `claims_namespace`: look up that key in the token's non-registered claims.
   - `claims_namespace_path`: evaluate the JSONPath against the non-registered claims object.
   - Not found: 400 `jwt-invalid-claims`:
     - `claims key: '<ns>' not found`
     - `claims not found at claims_namespace_path: '<path>'`
2. Format:
   - `json`: the value must be an object, else `expecting a json object when claims_format is json`.
   - `stringified_json`:
     - The value must be a string, else `expecting a string when claims_format is stringified_json`.
     - It is JSON-decoded into an object. On failure: `expecting stringified json at: 'claims_namespace <ns>', but found: <string>` (or `'claims_namespace_path <path>'`).
3. Keep only keys starting with `x-hasura-` (case-insensitive). Others are dropped.

`claims_map` mode:

```json
"claims_map": {
  "x-hasura-allowed-roles": {"path": "$.roles"},
  "x-hasura-default-role": {"path": "$.roles[0]", "default": "user"},
  "x-hasura-user-id": {"path": "$.sub"},
  "x-hasura-org": "acme"
}
```

- Each value is either a literal or `{"path": <JSONPath>, "default"?: <literal>}` (any JSON object is treated as the path form, so `path` is then required). Paths are evaluated against the FULL claims set (registered + unregistered), e.g. `$.sub`.
- `x-hasura-allowed-roles` and `x-hasura-default-role` are required keys in the map. They are parsed as a role list and a role.
- Missing at path with no default:
  - allowed or default role: 400 `jwt-missing-role-claims`, `JWT claim does not contain "x-hasura-allowed-roles"`;
  - other claims: 400 `jwt-invalid-claims`, `JWT claim from claims_map, "x-hasura-user-id" not found`.
- Wrong type at the role paths: 400 `jwt-invalid-claims`, `invalid "x-hasura-allowed-roles"; should be a list of roles: <aeson error>` / `invalid "x-hasura-default-role"; should be a role: <aeson error>`.
- Other claims: a literal must be a JSON string (parsed at startup). A path result is taken as raw JSON, so a non-string value found at the path fails later with `jwt-invalid-claims` `x-hasura-* claims: <aeson error>` (5.6 step 3). A `default` is a string. For the role keys the literal/`default` is a role list / role name.

### 5.6 Role and session variables

From the claims map:

1. `x-hasura-allowed-roles` must exist and parse as a list of role names. `x-hasura-default-role` must exist and be a role name.
   - Missing: 400 `jwt-missing-role-claims`, `JWT claim does not contain "x-hasura-allowed-roles"` (resp. `"x-hasura-default-role"`).
   - Wrong type: 400 `jwt-invalid-claims`, `invalid "x-hasura-allowed-roles"; should be a list of roles: <aeson error>` / `invalid "x-hasura-default-role"; should be a single role name: <aeson error>`.
     - Example aeson error: `parsing [] failed, expected Array, but encountered String`.
2. Requested role = the FIRST request header named `x-hasura-role` (`getRequestHeader` = `find`), if its value is a valid non-empty role name; else the default role. (An empty first occurrence falls back to the default role; a later non-empty duplicate is not consulted.)
   - If it is not in allowed roles: 400 `access-denied`, `Your requested role is not in allowed roles`.
   - The default role does not itself need to be in allowed roles unless it is used (inferred from the code).
3. Session variables = the remaining claims (without allowed and default role). They must all be JSON strings.
   - Non-string values give 400 `jwt-invalid-claims`, `x-hasura-* claims: <aeson error>`.
   - So `"x-hasura-user-id": 1` (a number) is an error. Array session vars must be encoded as a Postgres array literal string such as `"{1,2}"`.
4. `x-hasura-role` is set to the resolved role.
5. Request `x-hasura-*` headers other than `x-hasura-role` are NOT merged into the session in JWT mode.
6. The admin-secret state is "not sent", so the backend-only flag is always disallowed.

Discrepancy note: the source builds these messages with `<>>` / `<<>`, which wrap the variable name in double quotes. That gives `JWT claim does not contain "x-hasura-allowed-roles"` and `invalid "x-hasura-allowed-roles"; should be a list of roles: ...`. The `server/tests-py/test_jwt.py` and `test_jwt_claims_map.py` fixtures expect the unquoted form (`JWT claim does not contain x-hasura-allowed-roles`). Follow the source unless running the Python tests verbatim. This is listed as an open question.

### 5.7 JWK URL handling

- **Startup.** The engine fetches `jwk_url` with retries: exponential backoff from 200 ms, at most 5 retries (`Retry.exponentialBackoff 200000 <> limitRetries 5`).
  - HTTP errors, non-2xx responses and JWK parse errors are fatal.
  - Expiry-header parse errors are not fatal.
  - The startup fetch only validates; its result is discarded. The context starts with an empty key set.
- **Refresh thread.** `updateJwkCtxThread` (`App.hs`) loops forever, running immediately at start and then every 60 s. For each context with a URL:
  - empty key set: fetch now;
  - else, if an expiry is known and `now >= expiry`: fetch;
  - else: do nothing. If no expiry was ever determined, keys are never refreshed after the first successful fetch.
- **Fetch.**
  - `GET jwk_url`, with default headers `Content-Type: application/json` and `User-Agent: hasura-graphql-engine/<version>`.
  - Non-2xx: error `Non-2xx response on fetching JWK from: <url>`. HTTP exception: `Error fetching JWK: <reason>`. Parse failure: `Error parsing JWK from url: <url>`. In the background thread these are logged and the previous keys are kept.
  - The body is parsed robustly:
    - JWKs whose `use` is neither `sig` nor `enc` (nor missing) are filtered out.
    - RSA `n`/`e` values are normalised: standard base64 becomes base64url, padding is removed, and leading zero bytes are stripped (a Cognito workaround).
- **Expiry from response headers** (`determineJwkExpiryLifetime`):
  1. If `Cache-Control` is present:
     - `must-revalidate`: now + max-age if max-age is present, else now.
     - else `no-cache` or `no-store`: now (refetch on the next tick).
     - else `max-age` (or `s-maxage`, inferred): now + max-age.
     - else fall through to Expires.
  2. Else `Expires` (HTTP date): that time.
  3. Parse failures are logged (`Failed parsing Cache-Control header from JWK response`, `Failed parsing Expires header from JWK response. Value of header is not a valid timestamp`). On failure or absence of both headers, the new keys are stored but the previous expiry is kept.
- **Upstream bug to reproduce or fix (verified).** `updateJwkCtx` (`Server/Auth.hs`) pattern-matches only `AMAdminSecretAndJWT _ jwtCtxs _`; every other mode is `pure ()`. `updateJwkCtxThread` (`App.hs`) is the only caller and `mkJwtCtx` stores `JWKSet []` for a `jwk_url` config (the startup fetch result is discarded). So in `AMJWT` (disable-admin-secret + JWT) with `jwk_url`, the key set stays empty forever and every token fails verification (with jose 0.11 an empty key list gives `Could not verify JWT: JWSError NoUsableKeys`; inferred from jose, not tested upstream). Recommendation: implement refresh for both modes and record the deviation.

### 5.8 JWT error summary

| Code | Status | Messages |
|---|---|---|
| `invalid-headers` | 400 | `Malformed Authorization header`; `Missing 'Authorization' or 'Cookie' header in JWT authentication mode`; `Could not verify JWT: Multiple JWTs found` |
| `invalid-jwt` | 400 | `Could not verify JWT: <JWTError>` |
| `jwt-invalid-claims` | 400 | claims location and format errors (5.5), type errors (5.6), claims_map missing claim |
| `jwt-missing-role-claims` | 400 | `JWT claim does not contain "x-hasura-allowed-roles"` / `"x-hasura-default-role"` |
| `access-denied` | 400 | `Your requested role is not in allowed roles` |

All are 200 on `/v1/graphql` (section 9.2).

---

## 6. Webhook mode

Docs: `docs/docs/auth/authentication/webhook.mdx`. Code: `Server/Auth/WebHook.hs`.

### 6.1 GET mode (default)

```
GET <HASURA_GRAPHQL_AUTH_HOOK>
<all client request headers except the ignored list>
Content-Type: application/json
User-Agent: hasura-graphql-engine/<version>
```

- Ignored list (`commonClientHeadersIgnored`, compared case-insensitively): `Content-Length`, `Content-MD5`, `User-Agent`, `Host`, `Origin`, `Referer`, `Accept`, `Accept-Encoding`, `Accept-Language`, `Accept-Datetime`, `Cache-Control`, `Connection`, `DNT`, `Content-Type`.
- `x-hasura-*` headers ARE forwarded in GET mode, including `x-hasura-role`. In `AMAdminSecretAndHook` an `x-hasura-admin-secret` header never reaches the webhook: it is either valid (webhook skipped) or invalid (401). In `AMHook` it is forwarded unchanged.
- The default headers `Content-Type` and `User-Agent` replace any same-named header.

### 6.2 POST mode

```
POST <hook>
Content-Type: application/json
User-Agent: hasura-graphql-engine/<version>

{"headers": {"<name>": "<value>", ...}, "request": <GraphQL request>}
```

- `headers` holds ALL client request headers, with no filtering. Names keep their original casing; duplicates collapse with the last one winning.
- `request` is present only when `send_request_body` is true. It is the parsed GraphQL request as received: `{"query", "variables", "operationName"}`, or an array for batched requests.
- For non-GraphQL endpoints (`/v1/metadata`, `/v1/query`) and for WebSocket `connection_init`, the request is absent, so `"request": null` is sent when send_request_body is true.
- Docs: "If an invalid JSON request is sent, then the request body is not forwarded".

### 6.3 Response handling

| Webhook status | Result |
|---|---|
| 200 | Body must be a JSON object whose values are ALL strings (`HashMap Text Text`). Otherwise 500 `Invalid response from authorization hook: <aeson error>`. |
| 401 | 401 `access-denied`, `Authentication hook unauthorized this request` |
| anything else | 500 `Invalid response from authorization hook` |
| HTTP exception or timeout | 500 `webhook authentication request failed` |

On 200:

- Session variables are the body keys starting with `x-hasura-` (case-insensitive). Other keys are ignored.
- The role is the body's `X-Hasura-Role` (any case), and it is required. If missing: 400 `invalid-params`, `"x-hasura-role" not found in session variables`.
- The request's `x-hasura-role` header is IGNORED. Only the webhook decides the role (AuthSpec: "ignores requested role, uses webhook role").
- The backend-only flag is always disallowed.
- Expiry (used for WebSockets) comes from BODY keys, not response headers. The lookup is case-sensitive:
  - `"Cache-Control"`: `max-age=<n>`, giving now + n seconds;
  - else `"Expires"`: an HTTP date.
  - Parse failures are logged as warnings and ignored.
- Every `Set-Cookie` header of the webhook response is appended to the GraphQL response headers sent back to the client.

Example 200 body:

```json
{"X-Hasura-User-Id": "25", "X-Hasura-Role": "user", "X-Hasura-Is-Owner": "false", "Cache-Control": "max-age=600"}
```

Not in OSS:

- Webhook responses are not cached between HTTP requests. The webhook is called on every request. Caching via Redis is EE/Cloud (docs "Webhook Auth Caching").
- The webhook timeout is the HTTP manager default (inferred; there is no specific setting).

---

## 7. Headers and roles: more rules

### 7.1 `x-hasura-role`

| Mode | Effect of request header `x-hasura-role` |
|---|---|
| No auth | Selects the role (any string). Default `admin`. |
| Admin secret sent and valid | Selects the role (impersonation). Default `admin`. Other `x-hasura-*` headers become session variables. |
| Admin secret mode, no secret, unauthorized role | Ignored; the role is the unauthorized role. |
| JWT | Selects among allowed roles. Default from `x-hasura-default-role`. |
| Webhook | Ignored (forwarded to the webhook in GET mode or in POST `headers`). |

### 7.2 Unknown roles

Authentication does not check that a role exists in metadata. A role with no permissions gets the "unauthenticated" schema (04, section 9.3), so queries fail with GraphQL validation errors such as `field 'x' not found in type: 'query_root'`.

### 7.3 Client headers forwarded to other webhooks (for reference)

This is not auth itself, but shares the lists. For actions, remote schemas with `forward_client_headers` and `validate_input`, `mkClientHeadersForward` (`Authentication/Session.hs`) produces:

- `X-Forwarded-Host` from `Host`, `X-Forwarded-User-Agent` from `User-Agent` and `X-Forwarded-Origin` from `Origin`; plus
- all request headers except the `commonClientHeadersIgnored` list and except every `x-hasura-*` header.

Session variables travel separately in the webhook payload.

### 7.4 Logging redaction

`sensitiveHeaders` (`Authentication/Headers.hs`) is `Access-Token`, `Authorization`, `Cookie`. `redactSensitiveHeader` (`Authentication/Header.hs`) replaces their values with `<REDACTED>` where headers are logged (inferred: webhook and HTTP-client logs; the request-header logging in `Hasura/HTTP.hs` is commented out upstream).

---

## 8. CORS

Code: `Server/Cors.hs` and `Server/Middleware.hs` for HTTP; `GraphQL/Transport/WebSocket.hs` (`enforceCors`) for WebSocket.

### 8.1 Config

`HASURA_GRAPHQL_CORS_DOMAIN`:

- `*` (default): `CCAllowAll`.
- Otherwise, a comma-separated list. Entries are whitespace-trimmed. Each entry is `scheme://host[:port]` or `scheme://*.host[:port]`.
  - Scheme: `[A-Za-z0-9+.-]+` followed by `://`.
  - Error: `invalid domain: '<d>'. All domains should have scheme + (optional wildcard) host + (optional port)`.
  - Exact entries are stored as the string `scheme://host[:port]`, normalised by re-rendering. Wildcard entries are stored as `(scheme, host-without-*., port)`.

`HASURA_GRAPHQL_DISABLE_CORS=true`:

- gives `CCDisabled(readCookie)`.
- `HASURA_GRAPHQL_WS_READ_COOKIE=true` with CORS enabled is a startup error: `HASURA_GRAPHQL_WS_READ_COOKIE can only be used when CORS is disabled`.

Serialised (`/v1alpha1/config`, inferred): `{"disabled": bool, "ws_read_cookie": bool|null, "allowed_origins": "*" | {...}}`.

### 8.2 HTTP middleware

```
origin = request header "Origin"
if origin missing -> pass through, no CORS headers
match config:
  CCDisabled      -> pass through, no CORS headers
  CCAllowAll      -> sendCors
  CCAllowedOrigins:
     origin == one of the fqdns (exact string compare)   -> sendCors
     origin parses as scheme://<label>.<rest>[:port] and
       (scheme, rest, port) is in wildcards              -> sendCors
     otherwise -> pass through with NO CORS headers (request still served)

sendCors:
  if method == OPTIONS:
     204, empty body, headers:
       Access-Control-Allow-Origin: <origin>
       Access-Control-Allow-Credentials: true
       Access-Control-Allow-Methods: GET,POST,PUT,PATCH,DELETE,OPTIONS
       Access-Control-Expose-Headers: X-Hasura-Query-Cache-Key,X-Hasura-Query-Family-Cache-Key,Warning
       Access-Control-Max-Age: 1728000
       Access-Control-Allow-Headers: <value of request Access-Control-Request-Headers, or empty>
       Content-Length: 0
       Content-Type: text/plain charset=UTF-8
  else:
     run the handler, then prepend the four Access-Control-* headers above
     (Allow-Origin, Allow-Credentials, Allow-Methods, Expose-Headers)
```

Notes:

- The wildcard match strips exactly one leading label. So `https://*.example.com` matches `https://a.example.com` but not `https://a.b.example.com` and not `https://example.com`.
- Scheme and port must match exactly. No port in config means the origin must have no port.
- `Content-Type: text/plain charset=UTF-8` has no semicolon, as in the source.
- A disallowed origin over HTTP is not rejected. The browser blocks it client-side.

### 8.3 WebSocket handshake

- **Path.** Must be `/v1/graphql`, `/v1alpha1/graphql`, `/v1/relay` or `/v1beta1/relay`. Otherwise the handshake is rejected with HTTP 404 and body `{"errors":[{"extensions":{"path":"$","code":"not-found"},"message":"only '/v1/graphql', '/v1alpha1/graphql', '/v1/relay' and '/v1beta1/relay' are supported on websockets"}]}`.
- **Origin.** If an `Origin` header is present:
  - `CCAllowAll`: OK.
  - `CCDisabled(readCookie)`: if readCookie is false, the `Cookie` header is removed from the stored headers and this note is logged:

    > Cookie is not read when CORS is disabled, because it is a potential security issue. If you're already handling CORS before Hasura and enforcing CORS on websocket connections, then you can use the flag --ws-read-cookie or HASURA_GRAPHQL_WS_READ_COOKIE to force read cookie when CORS is disabled.

  - `CCAllowedOrigins`: same match as HTTP. If there is no match, the handshake is rejected with HTTP 400 and body `{"errors":[{"extensions":{"path":"$","code":"access-denied"},"message":"received origin header does not match configured CORS domains"}]}`.
- **Header filtering.** `sec-websocket-key`, `sec-websocket-version`, `upgrade` and `connection` are removed from the stored handshake headers.

Without an `Origin` header, nothing is checked and cookies are kept.

---

## 9. HTTP specifics

### 9.1 Pipeline order

For every API request (`mkSpockAction` in `Server/App.hs`):

1. CORS middleware (8.2).
2. Read the request headers. Authenticate: `getUserInfoWithExpTime(headers, authMode, parsedGraphQLRequest?)`.
   - For GraphQL POST, the body is parsed first so it can be sent to the POST webhook.
   - If authentication fails, the error is logged and returned. The handler does not run.
3. Webhook `Set-Cookie` headers are added to the response.
4. Run the handler with the `UserInfo`.

### 9.2 Status mapping on GraphQL endpoints

`POST /v1/graphql`, `GET /v1/graphql`, `POST /v1/relay` and `POST /v1beta1/relay` apply `maybeMod200`:

- every error status becomes HTTP 200;
- except: when `HASURA_GRAPHQL_PRESERVE_401_ERRORS=true`, status 401 stays 401.

`/v1alpha1/graphql` keeps the real status (e.g. 400 for JWT errors, 401 for admin-secret errors); the tests assert 400 there. `/v1/metadata`, `/v1/query`, `/v2/query`, `/v1/graphql/explain` and the others keep the real status.

### 9.3 Admin-only endpoints

| Endpoint | Check | Error (status 400) |
|---|---|---|
| `/v1/metadata` | role must be `admin` | `access-denied`, `restricted access : admin only`, path `$.args` |
| `/v2/query` | role must be `admin` | same |
| `/v1/query` | only for queries where `requiresAdmin` is true. DML `insert`, `select`, `update`, `delete`, `count` (and `bulk` of only those, inferred) are allowed for any role, with permissions applied. | same |
| `/v1/graphql/explain`, `/v1alpha1/graphql/explain`, `/v1alpha1/pg_dump`, `/v1alpha1/config`, `/dev/*` | `onlyAdmin` | `access-denied`, `You have to be an admin to access this endpoint` |

"Admin" means `role == admin`. So a request with admin secret plus `x-hasura-role: user` is NOT admin for these checks, and a JWT with role `admin` IS.

Example (`/v1/metadata` without admin):

```json
{"path":"$.args","error":"restricted access : admin only","code":"access-denied"}
```

---

## 10. WebSocket authentication

### 10.1 Protocols

Both `graphql-ws` (Apollo `subscriptions-transport-ws`, subprotocol `graphql-ws`) and `graphql-transport-ws` (the `graphql-ws` library) are supported. Details are in the subscriptions spec.

### 10.2 `connection_init`

```json
{"type": "connection_init", "payload": {"headers": {"Authorization": "Bearer <jwt>", "X-Hasura-Role": "user"}}}
```

- The payload is `ConnParams`: `{"headers": {string: string}}`. Only the `headers` key is read, and the payload and `headers` are optional. Headers placed directly at the payload top level (a common Apollo client mistake) are ignored.
- Auth headers = payload headers followed by the stored handshake headers (`mkHeaders st = paramHeaders ++ getClientHdrs st` in `onConnInit`, `GraphQL/Transport/WebSocket.hs`; handshake headers are after the CORS/cookie filtering of 8.3). Header names from the payload are case-insensitive. Consequences, all following from the HTTP code paths:
  - Non-JWT modes (no-auth, admin secret, webhook POST `headers` object, unauthorized role): session variables, `x-hasura-admin-secret` and `x-hasura-role` are read from `mkSessionVariablesHeaders` = `HashMap.fromList`, which is last-wins, so the UPSTREAM (handshake) header wins over the payload header with the same name.
  - JWT mode: the role is `getRequestHeader "x-hasura-role"` = `find` (first match), so the PAYLOAD role wins. Session variables come from the token, not headers.
  - JWT tokens: every `Authorization` header (and every matching cookie / custom header) is a candidate. If both the payload and the handshake carry a token that passes the issuer pre-match (even the same token twice), the result is 400 `invalid-headers` `Could not verify JWT: Multiple JWTs found`.
  - Webhook GET mode forwards the whole list (duplicates included) to the hook.
  - Admin-secret errors are the usual `invalid "x-hasura-admin-secret"` / `"x-hasura-admin-secret" required, but not found` (with the double quotes).
- Authentication then runs exactly like HTTP. In POST webhook mode, `"request": null`.
- On success:
  - The state becomes initialised with the `UserInfo`, the expiry and the payload headers (only the payload headers are kept as "request headers" for forwarding to actions and remote schemas).
  - The server sends `connection_ack`, then starts keep-alive.
- On failure: the state becomes `CSInitError <message>`. The server sends

  ```json
  {"type":"connection_error","payload":"<error message>"}
  ```

  and closes the socket:
  - Apollo (`graphql-ws` subprotocol): close code 4400, reason `4400: Connection initialization failed: <message>`.
  - `graphql-transport-ws`: close code 4403, reason `4403: Forbidden`.
- A second `connection_init` on an initialised connection re-authenticates using only the payload headers (handshake headers are no longer available: `getClientHdrs` returns `[]` unless the state is `CSNotInitialised`). This is upstream behaviour, noted as a TODO in the source.
- A failed init closes the socket (`sendCloseWithMsg`), so later messages are normally never processed. If one does arrive while the state is `CSInitError e` (race before the close completes), a further `connection_init` does not re-authenticate: `getIpAddress` returns `Left e` and the same `connection_error` + close is sent with the previous message.

### 10.3 Operations before or after a failed init

- `start`/`subscribe` before init: an error for that operation, `start received before the connection is initialised`.
- After a failed init: `cannot start as connection_init failed with: <message>`.
- Neither closes the connection.

### 10.4 Init timeout

If no `connection_init` arrives within the init timeout (`HASURA_GRAPHQL_WEBSOCKET_CONNECTION_INIT_TIMEOUT`, default 3 seconds), the connection is closed:

- `graphql-transport-ws`: close code 4408, reason `Connection initialisation timed out`.
- Apollo: close code 1011, same reason.

### 10.5 Token expiry

- On success the connection stores `expTime`. Its source depends on the mode:
  - JWT: from `exp`.
  - Webhook: from the `Cache-Control` / `Expires` body keys.
  - Admin secret and no-auth: none.
- A per-connection thread (`tokenExpiryHandler`) waits until the connection is initialised with an expiry, then sleeps until `expTime`.
- When it wakes, all connection threads are cancelled and the connection is torn down. The log event is `jwt_expired` (`EJwtExpired`). Upstream sends no GraphQL error message and no specific close code (inferred: the TCP connection is closed by the server library).
- The docs (webhook.mdx) say "a new request to the auth-webhook is made ... and a new websocket connection is established". In fact the server only closes the connection; reconnecting is the client's job.
- Recommendation for Rust: send close code 4401 or 1000 with reason "token expired" only if tests allow. Otherwise just close. Tests: `server/tests-py/test_jwt.py::test_jwt_expiry` (inferred: it checks that the connection closes).

### 10.6 Reaping on config change

When the auth mode, CORS policy, allowlist and several other settings change at runtime (e.g. metadata reload in EE), `websocketConnectionReaper` closes all connections. In OSS these settings are static, so this mostly matters for EE.

---

## 11. Examples

### 11.1 Admin secret mode

```
HASURA_GRAPHQL_ADMIN_SECRET=myadminsecretkey
```

| Request headers | Result |
|---|---|
| (none) | 401 → `/v1/graphql`: HTTP 200 `{"errors":[{"extensions":{"path":"$","code":"access-denied"},"message":"\"x-hasura-admin-secret\" required, but not found"}]}` |
| `x-hasura-admin-secret: wrong` | `invalid "x-hasura-admin-secret"` |
| `X-Hasura-Admin-Secret: myadminsecretkey` | role admin |
| `x-hasura-admin-secret: myadminsecretkey`, `x-hasura-role: user`, `x-hasura-user-id: 1` | role user, session `{x-hasura-role: user, x-hasura-user-id: 1}` |
| + `HASURA_GRAPHQL_UNAUTHORIZED_ROLE=anonymous`, no headers | role anonymous |

### 11.2 JWT mode

```
HASURA_GRAPHQL_ADMIN_SECRET=secret
HASURA_GRAPHQL_JWT_SECRET={"type":"HS256","key":"3EK6FD+o0+c7tzBNVfjpMkNDi2yARAAKzQlk8O2IKoxQu4nF7EdAh8s3TwpHwrdWT6R"}
```

Token payload:

```json
{"sub":"1","iat":1516239022,"exp":9999999999,
 "https://hasura.io/jwt/claims":{"x-hasura-allowed-roles":["user","editor"],"x-hasura-default-role":"user","x-hasura-user-id":"1"}}
```

- `Authorization: Bearer <token>`: role user, session `{x-hasura-role: user, x-hasura-user-id: "1"}`.
- Plus `x-hasura-role: editor`: role editor.
- Plus `x-hasura-role: admin`: `Your requested role is not in allowed roles`.

### 11.3 Webhook POST mode

```
HASURA_GRAPHQL_ADMIN_SECRET=secret
HASURA_GRAPHQL_AUTH_HOOK=http://auth:3000/webhook
HASURA_GRAPHQL_AUTH_HOOK_MODE=POST
```

The webhook receives `{"headers":{"Authorization":"Bearer abc","Content-Type":"application/json",...},"request":{"query":"...","variables":null,"operationName":null}}` and returns 200 `{"X-Hasura-Role":"user","X-Hasura-User-Id":"42"}`.

---

## 12. Implementation checklist (Rust)

1. Validate the startup mode table (section 3) with the exact fatal messages.
2. Session variable extraction from headers, with case-insensitive names lowercased.
3. `mkUserInfo` semantics, including the backend-only flag.
4. Admin secret: hash and compare in constant time (an improvement over upstream; same observable behaviour).
5. JWT:
   - config parse with exact errors;
   - token location;
   - issuer pre-match;
   - verification with jose-equivalent semantics and error names;
   - claims;
   - roles;
   - JWK refresh thread (60 s tick and expiry rules).
6. Webhook GET/POST with the header lists, response parsing, Set-Cookie passthrough and expiry from the body.
7. Error formatting and the 200 mapping (`PRESERVE_401_ERRORS`).
8. CORS middleware and the WebSocket handshake check.
9. WebSocket `connection_init` auth, failure close codes and the expiry-based close.

---

## Upstream tests to reuse

Unit tests (Haskell; port the cases):

- `server/src-test/Hasura/Server/AuthSpec.hs`: mode selection, admin secret, unauthorized role, webhook role override, JWT combinations, fatal startup errors. It is the most direct spec of section 3 and 4.
- `server/src-test/Hasura/Server/Auth/JWTSpec.hs`: JWT config parsing, JWK expiry determination and claims parsing.

Python integration tests (`server/tests-py/`), runnable against a drop-in engine:

- `test_jwt.py`: role selection, missing or invalid claims, expiry, signature, audience, issuer, `allowed_skew`, WebSocket expiry, cookie and custom header variants.
- `test_jwt_claims_map.py`: `claims_map` with path and default.
- `test_webhook.py`: GET/POST webhook flows, role from webhook, 401 handling.
- `test_webhook_request_context.py`: POST webhook receives the GraphQL request body.
- `test_auth_webhook_cookie.py`: `Set-Cookie` passthrough from the webhook.
- `test_webhook_insecure.py`: webhook HTTP failure (500 error).
- `auth_webhook_server.py`, `webhook.py`, `webhook/`: helper webhook servers for the above.
- `test_cors.py`: allowed and disallowed origins, wildcard, preflight headers.
- `test_websocket_init_cookie.py`: cookie handling with `--disable-cors` and `--ws-read-cookie`.
- `test_disable_admin_secret.py`: `HASURA_GRAPHQL_DISABLE_ADMIN_SECRET` behaviour.
- `queries/unauthorized_role/` (used by `test_graphql_queries.py`): unauthorized role access.
- The `check_query` helper in `server/tests-py/validate.py`: shows how tests expect `/v1/graphql` statuses (200) vs `/v1alpha1/graphql` (400) and how admin secret, JWT and webhook headers are added per mode.

api-tests: `server/lib/api-tests` has little auth-specific coverage. Most suites run in admin mode. Grep for `X-Hasura-Role`, `adminSecret` and `jwt` to find incidental cases.

Skip:

- Any test relying on `HASURA_GRAPHQL_ADMIN_SECRETS` / `HASURA_GRAPHQL_JWT_SECRETS` lists or on webhook caching. These are EE/Cloud only and not in the OSS test tree.
- SSO, license-key and "pro" suites.
- Data-connector-specific api-tests (`Test/DataConnector/*`).
