# Architecture

Shared Router is a single Rust binary built on Axum, with one SQLite database. It serves three friend-facing Anthropic Messages API routes, an owner dashboard with its JSON API, and two health probes. This page maps the modules and follows a `/v1/messages` request from arrival to final usage record. The code remains the source of truth; file references point into [`src/`](../src/).

## Modules

| Module | Responsibility |
|---|---|
| [`main.rs`](../src/main.rs) | CLI entry point. `serve` (default) loads config, opens the database, and runs the server; on Ctrl-C or SIGTERM it reports `/readyz` as 503, drains open requests for up to 20 seconds, records the rest as `interrupted`, and closes the database. `generate-key`, `hash-password` (piped input only), `backup PATH`, `healthcheck`, and `help` are one-shot commands; stray arguments exit with status 2. |
| [`lib.rs`](../src/lib.rs) | `AppState` (config, SQLite pool, HTTP client, OAuth state with the login throttle, and the admission semaphores with their limits) and the router: health probes, friend routes, admin routes, the 32 MiB body limit, and security headers on every response. |
| [`config.rs`](../src/config.rs) | Reads and validates environment variables at startup, and builds the upstream HTTP client with redirects and automatic retries disabled. |
| [`db.rs`](../src/db.rs) | Creates the database file (mode 600) and missing parent directories (mode 700), opens SQLite in WAL mode, runs embedded migrations, and performs startup recovery: unfinished requests become `interrupted` with an empty `finished_at`, and expired admin sessions are deleted. Also writes owner-only `backup` copies. |
| [`auth.rs`](../src/auth.rs) | Router key authentication (`x-api-key` or `Authorization: Bearer`, SHA-256 lookup), owner login with Argon2id and a login throttle, session cookies, the exact-origin check, and CSRF enforcement for admin mutations. |
| [`policy.rs`](../src/policy.rs) | The Fable 5.1 block, model resolution through grants and aliases, the request body allowlists, and `ToolMap`, which renames custom tools to hashed `custom_` names on the way up and restores them on the way down. |
| [`proxy.rs`](../src/proxy.rs) | Friend routes: `/v1/messages`, `/v1/messages/count_tokens`, and `/v1/models`. Runs the request pipeline below, the `anthropic-beta` allowlist, the SSE decoder, and response model verification. |
| [`oauth.rs`](../src/oauth.rs) | Claude OAuth PKCE login and completion, token encryption with XChaCha20-Poly1305 under `ENCRYPTION_KEY`, serialized access-token refresh, the `needs_reauth` state, and the fixed upstream headers. |
| [`usage.rs`](../src/usage.rs) | Per-request usage rows: start, allowlisted token checkpoints, final outcome, and a drop guard that records `interrupted` if a handler ends early. |
| [`analytics.rs`](../src/analytics.rs) | Read-only usage reports for `/admin/api/usage` and `/admin/api/analytics`: filters, grouping, totals, and paginated request history. |
| [`admin.rs`](../src/admin.rs) | Dashboard pages and static assets, `/readyz`, and the admin JSON API for people, keys, grants, the model catalog, and aliases. |
| [`error.rs`](../src/error.rs) | `AppError`, the Anthropic-style JSON error envelope, and the database error conversion that logs only a fixed message. |
| [`tests/`](../src/tests/) | Integration tests against temporary SQLite databases and mock upstream servers: a shared harness (`harness.rs`), a mock Anthropic upstream (`mock_upstream.rs`), and one module per area. |

## Request flow for `/v1/messages`

```mermaid
sequenceDiagram
    autonumber
    participant C as Client (opencodex)
    participant P as proxy::forward
    participant A as auth
    participant U as usage (SQLite)
    participant Pol as policy
    participant S as Admission (3 per key, 8 global)
    participant O as oauth
    participant Up as api.anthropic.com

    C->>P: POST /v1/messages
    P->>A: proxy::authenticate: api_key(headers), before the body is read
    A-->>P: key id, or 401
    P->>P: read and parse the JSON body (at most 32 MiB)
    P->>U: start: insert request_usage row (in_progress)
    P->>Pol: validate(body), resolve(key, model), check anthropic-beta
    Pol-->>P: resolved model id, or 400/403 (row finished as denied)
    P->>Pol: ToolMap::prepare (hash custom tool names, prepend system block)
    P->>S: try_acquire key permit, then global permit (no waiting)
    S-->>P: permits, or immediate 429 with retry-after: 1 (denied)
    P->>O: access()
    O->>O: lock, decrypt, refresh if expiring within 5 minutes
    O-->>P: access token, or 503 reconnect Claude
    P->>Up: one POST (a /v1/messages request is never replayed)
    alt Upstream 401
        Up-->>P: 401
        P->>O: force_refresh, once
        O-->>P: new token, or 503 reconnect Claude if the refresh is rejected
        P-->>C: 503 "retry the request" (only count_tokens is resent with the new token)
    else Other upstream error status
        Up-->>P: 4xx/5xx
        P-->>C: sanitized error (a 403 authentication_error marks needs_reauth)
    else Streaming (stream: true)
        loop Each SSE event
            Up-->>P: event
            P->>U: checkpoint usage on message_start and message_delta
            P->>Pol: restore original tool names
            P-->>C: event
        end
        P->>U: finish completed on message_stop, or interrupted if the client leaves
    else Non-streaming
        Up-->>P: JSON message
        P->>U: checkpoint and finish completed
        P-->>C: JSON with tool names restored
    end
```

The steps in order:

1. **Authentication.** The `proxy::authenticate` middleware runs `auth::api_key` on the headers alone, before any of the body is read, so requests without a valid key never cause the router to buffer or parse up to 32 MiB. `api_key` accepts exactly one `x-api-key` or `Authorization: Bearer` value (both are allowed only if they match), requires the `sr_` format, looks up the SHA-256 hash among unrevoked keys, and updates `last_used_at` at most once a minute per key. Only then does the handler read and parse the JSON body, within the 32 MiB limit.
2. **Usage row.** `usage::start` records the attempt as `in_progress`. A `RequestGuard` ensures the row reaches a terminal outcome even if the handler is cancelled.
3. **Policy.** `policy::validate` enforces the request field allowlists (see the README's accepted request surface). `policy::resolve` maps the requested ID or explicit alias to a reviewed, enabled model granted to this unrevoked key and rejects Fable 5.1. Caller `anthropic-beta` values must all be in `proxy::CLIENT_BETAS` or the router's own `oauth::BETA`. Any failure finishes the row as `denied`.
4. **Rewrite.** `ToolMap::prepare` renames custom tools and matching `tool_use` blocks to stable hashed `custom_` names and prepends the required system block. Server tools keep their names.
5. **Admission.** `proxy::acquire` takes the key's permit first (`PER_KEY_CONCURRENCY`, 3 per key) and then a global one (`GLOBAL_CONCURRENCY`, 8 across all keys). Token counts take a permit from their own pool (`COUNT_TOKENS_CONCURRENCY`, 4) instead. The router does not queue: when a pool is full the request is finished as `denied` and gets an immediate `429 rate_limit_error` with `retry-after: 1`, saying "This key has too many requests in progress" for the per-key limit and "The router is busy" otherwise. A streaming response holds its permits until the stream ends.
6. **Credential.** `oauth::access` decrypts the stored tokens. A token that expires more than 5 minutes from now is used without locking; otherwise one caller at a time refreshes it under the refresh lock, and a generation counter guards the write. A refresh rejected with 400, 401, or 403 marks the credential `needs_reauth`, and requests get `503` until the owner reconnects. A transient refresh failure starts a 30-second backoff, during which the current token is used while it is still valid. A credential that cannot be decrypted is also marked `needs_reauth`.
7. **Upstream call.** The router sends one POST with the owner's token and fixed client headers. Incoming credentials are never forwarded. The HTTP client has retries and redirects disabled, and no code path replays a `/v1/messages` request, including after 429s, network errors, or a 401.
8. **Upstream 401.** `oauth::force_refresh` refreshes the token once; if another request has already replaced that token generation, it reuses the newer token instead. A token count is sent again with the new token. A `/v1/messages` request is finished as `upstream_error` and its client gets a retryable `503 api_error` ("The router renewed its Claude session. Retry the request"). Only a rejected refresh, or a second 401 for the resent token count, marks the credential `needs_reauth`.
9. **Response.** Other error statuses return a fixed message with the upstream status and its matching Anthropic error type (redirects become 502), and `retry-after` is passed through. A 403 marks the credential `needs_reauth` only when its `error.type` is `authentication_error`; the body is read up to 64 KiB and never logged or returned. Successful responses are checked against the resolved model; a mismatch becomes 502.
10. **Accounting.** Streaming usage is checkpointed before each usage-bearing event is forwarded. Cumulative counters replace earlier values. The row is finished once as `completed`, `upstream_error`, or `interrupted`. Only the four allowlisted numeric token fields are stored.

`/v1/messages/count_tokens` follows the same path with `counting` set: `max_tokens` is optional, streaming is refused, and usage is recorded as `not_applicable`, which keeps it out of consumed-token reports. `/v1/models` authenticates the key and lists its granted, reviewed, enabled models, filtering out Fable 5.1.

## Process-local coordination

Three pieces of state live in the process: the OAuth mutex that serializes login and refresh, the admission semaphores, and the login throttle (five failed sign-ins per client address and 30 in total per minute). The client address is the socket peer, or, when `TRUSTED_PROXY_HOPS` is N greater than 0, the N-th `X-Forwarded-For` entry from the right; `Config` reads the setting at startup and refuses to start when it is not a non-negative integer. Pending OAuth logins live there too. Startup recovery also assumes that no other process is writing `in_progress` rows. A second replica on the same database would double the admission limit, race refresh-token rotation, and mark the other replica's live requests as interrupted when it starts. Run exactly one router process per database.

## Admin surface

Admin routes live in `admin.rs`. Everything under `/admin/api/` except `login` passes through `auth::require_admin`: a valid session cookie is required, and methods other than GET and HEAD also need the exact configured `Origin` and a matching `X-CSRF-Token`. Admin bodies are capped at 64 KiB. Friend keys never authorize admin routes.
