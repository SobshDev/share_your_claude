# Shared Router

A private Claude gateway for a small group. Friends use individual router keys in opencodex; the owner’s Claude OAuth credentials remain on the server. Fable 5.1 is blocked for every router key and cannot be granted; the owner reaches it only through direct Claude access, outside this router.

The Rust service includes an owner dashboard, model grants, streaming and non-streaming Messages API support, token counting, model discovery, encrypted OAuth credentials, coordinated refresh, and SQLite usage reports. It does **not** impose token budgets or convert token counts into subscription-limit percentages.

## Run locally

Requires Rust 1.94+ and a C compiler (SQLite is bundled). HTTP is accepted only for a loopback `PUBLIC_ORIGIN`; use HTTPS elsewhere.

```bash
cargo build --locked
export ENCRYPTION_KEY="$(./target/debug/shared-router generate-key)"
```

Create the owner password hash without putting the password in shell history or process arguments (Bash):

```bash
read -r -s -p 'Owner password: ' router_password; echo
export ADMIN_PASSWORD_HASH="$(printf '%s' "$router_password" | ./target/debug/shared-router hash-password)"
unset router_password

PUBLIC_ORIGIN=http://localhost:8080 ./target/debug/shared-router
```

Open http://localhost:8080/admin. Keep `PUBLIC_ORIGIN` identical to the browser origin, including its port. The default bind address is `127.0.0.1:8080`; the database defaults to `data/router.sqlite`. The router creates the database file with mode 600, and any missing parent directories with mode 700, on first start.

### Commands

| Command | Behavior |
|---|---|
| `shared-router` or `shared-router serve` | Runs the router. |
| `shared-router generate-key` | Prints a new random `ENCRYPTION_KEY`. |
| `shared-router hash-password` | Reads the password from piped standard input and prints its Argon2id `ADMIN_PASSWORD_HASH`. It refuses to read from a terminal, where the password would be echoed. Trailing line breaks are ignored, and the password must be 12 to 1024 bytes. |
| `shared-router backup PATH` | Writes a consistent copy of the database named by `DATABASE_URL` to a new file at `PATH` (mode 600) with SQLite `VACUUM INTO`. It is safe while the router is serving, and it never overwrites an existing file. |
| `shared-router healthcheck` | Requests `/readyz` from the router at `BIND_ADDRESS` (an unspecified address such as `0.0.0.0` becomes loopback) with a 5-second timeout. Exits 0 when ready and 1 otherwise. The Docker image uses it as its health check. |
| `shared-router help`, `-h`, `--help` | Prints usage and exits 0. |

Any other command, or extra arguments to one of the commands above, prints the usage to standard error and exits with status 2.

### Configuration

The router reads these environment variables at startup; `serve` refuses to start with a message naming the variable when a value is missing or invalid. The container column shows the value set by the `Dockerfile` or `compose.yaml` when the variable is not set in the environment.

| Variable | Required | Default | Container | Notes |
|---|---|---|---|---|
| `PUBLIC_ORIGIN` | Yes | none | none | The exact browser origin, such as `https://router.example.com`: scheme, host, and port, with no path, query, or credentials. HTTPS is required except on `localhost`, `127.0.0.1`, and `[::1]`. Used for the Origin and CSRF checks; HTTPS also marks the session cookie `Secure` and turns on `Strict-Transport-Security`. |
| `ENCRYPTION_KEY` | Yes | none | none | 32 random bytes, base64-encoded (`shared-router generate-key`). Encrypts the stored Claude tokens; changing it makes them unreadable and requires reconnecting Claude. |
| `ADMIN_PASSWORD_HASH` | Yes | none | none | Argon2id hash in PHC format (`shared-router hash-password`, password 12 to 1024 bytes). Quote it in `.env` and Dokploy so its `$` characters stay literal. Changing it signs out every dashboard session. |
| `BIND_ADDRESS` | No | `127.0.0.1:8080` | `0.0.0.0:8080` | IP address and port to listen on; host names are not accepted. `healthcheck` probes the same port. |
| `DATABASE_URL` | No | `sqlite://data/router.sqlite` | `sqlite:///data/router.sqlite` | SQLite URL. `sqlite://` followed by a relative path is resolved against the working directory; three slashes make it absolute. Missing parent directories are created with mode 700. `backup` reads the same variable. |
| `RUST_LOG` | No | `shared_router=info` | `shared_router=info` | [`tracing` filter](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html) for log output. An invalid value falls back to the default. |
| `TRUSTED_PROXY_HOPS` | No | `0` | `0` | Number of reverse proxies in front of the router that append to `X-Forwarded-For`, as a non-negative integer. With N greater than 0, sign-in throttling takes the N-th `X-Forwarded-For` entry from the right as the client address; `0` uses the TCP peer. Set `1` behind Dokploy's Traefik. An empty value means `0`; any other value that is not a non-negative integer stops startup. |

`compose.yaml` forwards `PUBLIC_ORIGIN`, `ENCRYPTION_KEY`, `ADMIN_PASSWORD_HASH`, `RUST_LOG`, and `TRUSTED_PROXY_HOPS` from the Compose environment (Dokploy's Environment settings or `.env`); `BIND_ADDRESS` and `DATABASE_URL` come from the image. A variable reaches the container only if `compose.yaml` lists it under `environment`.

## Deploy on Dokploy

Create a **Docker Compose** service in Dokploy, connect this repository, and select `compose.yaml`. Use Compose mode rather than Docker Stack, since this file builds the included Dockerfile.

1. Set `PUBLIC_ORIGIN=https://router.example.com` in the service's Environment settings, using your actual domain. This exact browser origin is used for secure cookies and CSRF checks. Also set `TRUSTED_PROXY_HOPS=1`, so sign-in throttling counts each client separately instead of treating Traefik as the only client.
2. Generate production secrets locally using the commands below, or the native binary commands in the local setup section.
3. Paste the generated `ENCRYPTION_KEY` and `ADMIN_PASSWORD_HASH` lines directly into Dokploy's **Environment** settings. These are secret **values**, not paths. Keep the single quotes in the environment editor so the hash's `$` characters remain literal. No secret files or mounts are required.
4. In **Domains**, add your domain for service **router**, container port **8080**, path **/**, and enable HTTPS. Dokploy supplies the proxy routing and certificate; the container serves HTTP internally.
5. Deploy, open `https://your-domain/admin`, and connect Claude through the dashboard.

```bash
docker build -t shared-router:local .
bash scripts/bootstrap-secrets.sh
```

The bootstrap script asks for a password and prints ready-to-paste environment entries without creating files. `ENCRYPTION_KEY` is a random 32-byte key encoded as base64. `ADMIN_PASSWORD_HASH` is an Argon2id hash of the password you will use to sign in; it is not another random string. The binary's `generate-key` and `hash-password` commands generate these values individually too.

Generate the encryption key once, keep it stable across redeployments, and back it up securely. Replacing it makes saved Claude credentials unreadable. Keep production values out of the repository.

Compose exposes port 8080 only to the container network, with no host-port bindings. Configure the domain in Dokploy's UI; it adds the required routing labels and network automatically. See [Dokploy Compose domains](https://docs.dokploy.com/docs/core/docker-compose/domains). Keep streaming responses unbuffered if you add any custom proxy middleware.

When `PUBLIC_ORIGIN` is HTTPS, every response carries `Strict-Transport-Security: max-age=31536000`, so browsers use only HTTPS for that host for a year after their first visit. Serve the domain over HTTPS before deploying with an HTTPS origin, and keep it that way. Dashboard pages and API responses are never cached (`Cache-Control: no-store`). The pages link the stylesheet and scripts as `/assets/…?v=<hash>`, where the hash covers every embedded asset, so those URLs change with each release that changes an asset and are cached for a year (`public, max-age=31536000, immutable`). Any other asset request, such as a module imported without the version, gets `no-cache` and is revalidated.

The router runs as UID 10001 with a read-only root filesystem and a persistent `router_data` volume at `/data`. Run **one router process/replica per database**: refresh coordination and admission control are process-local, and startup recovers unfinished requests. Preserve this volume across redeployments.

`compose.yaml` also caps the container's resources: Docker's `json-file` logs rotate at 10 MB and keep five files, memory is limited to 512 MB, and the process count to 256. Logging defaults to `RUST_LOG=shared_router=info`; set `RUST_LOG` in Dokploy's Environment settings to override it, for example `shared_router=debug` while troubleshooting.

To try the container locally, copy `.env.example` to `.env`, set `PUBLIC_ORIGIN=http://localhost:8080`, and fill in `ENCRYPTION_KEY` and `ADMIN_PASSWORD_HASH` with the values from `scripts/bootstrap-secrets.sh`. Keep the single quotes around the password hash in `.env`; when exporting it in a shell, quote it there as well. Then start the stack with the local override, which publishes the router on `127.0.0.1:8080` only, and open http://localhost:8080/admin:

```bash
docker compose -f compose.yaml -f compose.local.yaml up --build
```

The production `compose.yaml` on its own publishes no host ports.

`GET /healthz` checks the process; `GET /readyz` checks SQLite. Readiness does not require an active Claude login, so initial setup can be completed through the dashboard. The image's health check runs `shared-router healthcheck`, which requests `/readyz` on the configured port and exits nonzero when the router is not ready.

On SIGTERM or Ctrl-C the router answers `/readyz` with 503 at once, so the proxy stops sending new requests, and lets open requests finish for up to 20 seconds. Then each open stream saves the usage seen so far, is recorded as `interrupted`, and ends with an error event. After up to 3 more seconds, any request still open is recorded as `interrupted` with the shutdown time, and the router spends up to 5 seconds closing the database. That adds up to 28 seconds; `compose.yaml` sets `stop_grace_period: 30s` to cover it, so keep it at 30 seconds or more or Docker will kill the process first.

### Releases and upgrades

Releases are tagged `vX.Y.Z`, and [CHANGELOG.md](CHANGELOG.md) lists what each one changes, including any configuration steps and new migrations. Read it before upgrading, and take a backup first (see the [operations runbook](docs/operations.md)).

Deploying from `main` picks up every change as it lands. To deploy only releases from a GitHub source, keep **Auto Deploy** on in the Compose service and set its **Trigger Type** to **On Tag**; Dokploy then deploys the tagged commit whenever a new tag is pushed. Confirm after the first push that ordinary commits no longer deploy, because some Dokploy versions have ignored this setting. The tag trigger reacts to new tags and does not hold a deployment on one, so to stay on or return to a specific release, point a deployment branch at that tag and select the branch in Dokploy:

```bash
git push --force origin 'v0.1.0^{commit}:refs/heads/deploy'
```

## First connection and friend setup

1. Sign in with the owner password, open **Claude connection**, and click **Connect Claude**.
2. Open the generated Claude authorization link in your browser. After authorization, copy the entire `http://localhost:54545/callback?code=…&state=…` address into the dashboard. The browser may show a connection error at that address; copying it still completes the flow. Keep `localhost` exactly as generated: the OAuth client does not accept `127.0.0.1` as a substitute. State is session-bound and expires after ten minutes.
3. Refresh the catalog. Review and enable the models you want to share. Discovery alone never enables a new model. Fable 5.1 stays blocked.
4. Open **Friends & keys**, add a person, and create their key. Copy the key immediately: only its hash is stored, and it cannot be recovered later.
5. Use **opencodex setup** beside the key to copy its provider configuration. Merge it into the friend’s opencodex configuration. Set `SHARED_CLAUDE_API_KEY` in the environment of the opencodex process, then restart that process. A background service must receive that environment variable too; alternatively put the issued router key in the local provider’s `apiKey` field and protect the configuration file.

```json
{
  "providers": {
    "shared-claude": {
      "adapter": "anthropic",
      "baseUrl": "https://router.example.com",
      "authMode": "key",
      "apiKey": "${SHARED_CLAUDE_API_KEY}",
      "models": ["claude-sonnet-4-6"]
    }
  }
}
```

The displayed configuration uses only models granted to that key. Even if a client invents another model name, the router checks access again before contacting Claude. Revocation blocks new requests; already-admitted requests can finish. New keys start with every currently enabled model except Fable 5.1. Enabling a model later does not change existing keys; use **Edit access**.

If friends previously received your actual account credentials, revoke those sessions/credentials before relying on router restrictions. Personal requests sent directly to Claude are outside this router’s per-person accounting. Blocking Fable does not reserve shared account capacity.

## API contract

Friend endpoints accept `x-api-key: sr_…` or `Authorization: Bearer sr_…`. Conflicting/duplicate authentication headers are rejected. Request bodies are capped at 32 MiB.

| Method | Path | Result |
|---|---|---|
| POST | `/v1/messages` | Anthropic JSON or SSE response |
| POST | `/v1/messages/count_tokens` | Estimate; excluded from consumed-token reports |
| GET | `/v1/models` | Permitted reviewed model catalog |

No batch, arbitrary forward-proxy, Files, Managed Agents, or provider-management routes are exposed to friends. Unreviewed request fields, beta headers, server tool types, and fallback/advisor routing are rejected. Custom client tools, images, thinking, and cache controls are supported. Incoming credentials are replaced with the owner’s upstream token. Inference requests are never automatically replayed, including after 429s, network errors, or an upstream 401.

Admission control keeps one friend from taking the whole router. Each key may have 3 `/v1/messages` requests in progress, and at most 8 run at once across all keys. Token counts use a separate pool of 4, so they never wait behind long streams. The router does not queue: a request beyond a limit is answered immediately with `429 rate_limit_error` and `retry-after: 1`, saying either "This key has too many requests in progress" or "The router is busy". A streaming response holds its slots until the stream ends.

Upstream calls have fixed time limits, which are constants in the code rather than settings. A stream may run for up to 60 minutes; after that the router sends a `timeout_error` event, ends the stream, and records the request as `interrupted`. A non-streaming `/v1/messages` call times out after 600 seconds, a token count after 60 seconds, and each catalog page fetched by **Refresh from Claude** after 30 seconds; a timed-out call returns `502 api_error` and is not retried. Every upstream connection must also open within 15 seconds and must not stall for more than 120 seconds between reads.

See [docs/architecture.md](docs/architecture.md) for the module map and the full request flow.

### Accepted request surface

Anything outside these lists is rejected with a 400 `invalid_request_error`. The allowlists live in [`src/policy.rs`](src/policy.rs) (`validate`), [`src/proxy.rs`](src/proxy.rs) (`CLIENT_BETAS`), and [`src/oauth.rs`](src/oauth.rs) (`BETA`, the router's own betas), which are the source of truth.

| Part | Accepted values |
|---|---|
| Top-level body fields | `model`, `messages`, `system`, `tools`, `tool_choice`, `max_tokens`, `stream`, `temperature`, `top_p`, `top_k`, `stop_sequences`, `metadata`, `thinking`, `output_config`, `cache_control`, `service_tier` |
| `thinking` keys | `type`, `budget_tokens`, `display` |
| `output_config` keys | `effort`, `format` |
| `tool_choice` keys | `type`, `name`, `disable_parallel_tool_use` |
| `metadata` keys | `user_id` |
| `cache_control` keys (top level) | `type`, `ttl` |
| Tool definition keys | `name`, `type`, `description`, `input_schema`, `cache_control`, `strict`, `defer_loading`, `allowed_callers`, `max_uses`, `allowed_domains`, `blocked_domains`, `user_location`, `citations`, `max_content_tokens`, `display_width_px`, `display_height_px`, `display_number` |
| Tool `type` values | omitted or `custom`, `web_search_20250305`, `web_fetch_20250910`, `code_execution_20250522`, `code_execution_20250825`, `text_editor_20250124`, `text_editor_20250429`, `text_editor_20250728`, `computer_20250124`, `bash_20250124` |
| `anthropic-beta` header values | Caller betas (`proxy::CLIENT_BETAS`): `prompt-caching-2024-07-31`, `interleaved-thinking-2025-05-14`, `fine-grained-tool-streaming-2025-05-14`. The router's own betas (`oauth::BETA`), `claude-code-20250219` and `oauth-2025-04-20`, may also be listed. |

`model` is required (1–200 bytes) and `messages` must be an array. `/v1/messages` requires a positive integer `max_tokens`; `stream` must be a boolean, and `count_tokens` refuses `stream: true`. Each tool must be an object with a name of 1–128 bytes, unique within the request. The contents of messages, system blocks, and tool input schemas are passed through as data. Betas may be split across several `anthropic-beta` header lines and comma-separated within each, and every one must be on the list. The router always sends `claude-code-20250219,oauth-2025-04-20` upstream, followed by the accepted caller betas, each listed once.

Errors from friend and admin API routes use Anthropic's envelope, `{"type":"error","error":{"type":…,"message":…}}`, with the error type that matches the status. Messages are fixed strings chosen by the router; upstream error text is never relayed. A body that is not valid JSON gets "Invalid JSON request body", a rejected query string, such as a negative `offset` on `/admin/api/analytics`, gets "Invalid query parameters", and any other malformed request gets "Invalid request". When a non-streaming reply names a different model than the one the router resolved, the client gets `502 api_error` "Claude answered with a different model than requested"; a stream that does the same ends with an `api_error` event. The dashboard and sign-in pages are HTML, so when one cannot be rendered the router returns a small HTML 500 page asking you to reload or check the logs.

The dashboard API uses session cookies, exact-origin checks, and `X-CSRF-Token` for mutations. `POST /admin/api/login` takes `{"password":"…"}`, requires the configured Origin, and returns the CSRF token; `GET /admin/api/me` returns it for an existing session. Sessions expire after twelve hours. Each session is bound to the `ADMIN_PASSWORD_HASH` it was issued under, so changing that value and redeploying signs every dashboard session out. Friend API keys never authorize admin operations.

Sign-in allows five failed attempts per client address per minute, and 30 failed attempts per minute across all clients, so a stranger guessing passwords cannot lock the owner out from another address. A successful sign-in does not count and clears that address's failures. Once an address reaches the limit, it gets 429 until its one-minute window ends, even with the right password. IPv6 addresses are grouped by /64. The counters live in memory and reset on restart. By default the client address is the TCP peer; behind a reverse proxy every request comes from the proxy, so set `TRUSTED_PROXY_HOPS` to the number of proxies in front of the router that append to `X-Forwarded-For` (`1` behind Dokploy's Traefik). The router then uses the N-th entry counted from the right, where N is that setting. Do not set it when clients connect directly, because they could then choose their own address.

| Method | Path | Input / behavior |
|---|---|---|
| GET / POST | `/admin/api/people` | List / create `{"name":"Alex"}` |
| PATCH | `/admin/api/people/{id}` | Rename `{"name":"Alex"}` |
| GET / POST | `/admin/api/keys` | List / issue `{"person_id":"…","label":"Laptop"}`; secret returned once |
| DELETE | `/admin/api/keys/{id}` | Revoke, retaining history |
| PUT | `/admin/api/keys/{id}/models` | Replace grants `{"models":["claude-sonnet-4-6"]}` |
| GET | `/admin/api/keys/{id}/config` | Generated opencodex configuration |
| GET | `/admin/api/models` | Reviewed and pending catalog |
| POST | `/admin/api/models/refresh` | Discover upstream candidates |
| PUT | `/admin/api/models/{id}` | Review/enable or disable `{"enabled":true}` |
| POST | `/admin/api/models/{id}/aliases` | Create explicit alias `{"alias":"shared-sonnet"}` |
| POST | `/admin/api/claude/login` | Start PKCE flow; return authorization URL |
| POST | `/admin/api/claude/complete` | `{"redirect_url":"http://localhost:54545/callback?…"}` |
| GET | `/admin/api/usage` | `group_by=person\|key\|model\|day`, optional RFC3339 `from`/`to`, `person_id`, `key_id`, `model` |
| GET | `/admin/api/analytics` | Totals, person/model/key/day breakdowns, and 50 requests per page; same filters as usage, plus `offset` |
| POST | `/admin/api/logout` | Delete current session |

Usage ranges are `[from,to)` in UTC; the API defaults to the last 30 days. The dashboard’s Through date is inclusive. Disabling a catalog model immediately blocks it for all keys without deleting historical grants. Explicit aliases cannot replace canonical model IDs.

The usage overview supports all users or an individual user, a model filter, custom UTC dates, and 7/30/90-day shortcuts. It includes daily token/request/error charts, model and user share pies, token-category and outcome pies, a grouped ledger with selection totals, and paginated request history. Click a user in the ledger or share chart, or use **View usage** in Friends & keys, to inspect that person's activity across keys (including revoked keys).

All charts and tables share the applied filters. Model reporting groups by the resolved routing model, falling back to the requested model for unresolved attempts; request history also shows a differing response model. Models routed excludes denied and unresolved requests. Completed percentage includes all message attempts in its denominator; average duration covers completed requests from start to finish, including streaming. Count-token estimates are excluded. Null token totals stay unknown; observed partial counts remain flagged. User/model share percentages describe the selected metric within this router, not subscription capacity or monetary cost.

## Accounting and failure behavior

SQLite stores people, hashed keys, reviewed models/aliases, grants, an encrypted credential, admin sessions, and per-request usage. The schema is the ordered set of files in [`migrations/`](migrations/).

```text
Person 1 ── N ApiKey N ── N Model (through KeyModelGrant)
                  │           └── N ModelAlias
                  └── N RequestUsage
ClaudeCredential: one encrypted account
AdminSession: hashed session token + CSRF token + expiry
```

`input_tokens` excludes cache reads/writes. Observed total = input + cache read + cache write + output. Cumulative streaming snapshots replace previous counts; they are not added. The router checkpoints usage before forwarding usage-bearing events and finalizes once. Tool names are mapped with stable hashed `custom_` names and restored only at protocol tool-use locations; original names that already start with `custom_` remain distinct.

Usage states are `complete`, `partial`, `unknown`, and `not_applicable`. Stream cancellation aborts the upstream connection and preserves the last observed counts. Missing counters are never estimated from text. Reports include observed partial counts and show incomplete request counts; an em dash means unreported, not zero. Requests interrupted before the next checkpoint, or while the process is unavailable, cannot have exact totals reconstructed from the subscription. Completed usage describes what Anthropic reported, even if the final client delivery fails.

On a clean shutdown, requests still open after the 20-second drain period are recorded as `interrupted` with the shutdown time; open streams first save the counts they have seen. After a crash or kill, the next startup marks the leftover `in_progress` rows as `interrupted` and leaves their `finished_at` empty, because the time the process stopped is unknown. Both keep the last checkpointed counts.

Prompts, completions, raw error bodies, API keys, and OAuth tokens are not logged or stored as usage. Only allowlisted numeric usage fields are persisted. Logs report operation failures without SQL values or provider response bodies. Do not enable HTTP body tracing or configure a reverse proxy to log credentials.

Report suspected vulnerabilities privately as described in [SECURITY.md](SECURITY.md), never in a public issue.

When Claude rejects the owner's access token with a 401, the router refreshes the token once. A token count, or a catalog page requested by **Refresh from Claude**, is then sent again with the new token. A `/v1/messages` request is not replayed: its client gets a retryable `503 api_error` ("The router renewed its Claude session. Retry the request") and sends it again itself. The account is marked as needing reconnection only when the refresh itself is rejected (400, 401, or 403 from the token endpoint), when a resent token count or catalog page is still refused with 401, when an upstream 403 has type `authentication_error`, or when the stored credential cannot be decrypted. Any other 403 concerns that one request, such as a model the account cannot use, and is returned as `permission_error` without touching the connection; for a catalog refresh the dashboard shows "Claude did not allow listing models". A transient refresh failure backs off for 30 seconds and keeps using the current token while it is still valid; the router never falls back to billed API access. The OAuth compatibility behavior is based on opencodex 2.49.0; live provider requirements can change. Model routing is restricted to the reviewed schema rather than passing new provider features through automatically.

## Backup and recovery

On Dokploy, you can run `shared-router backup /data/router-backup.sqlite` in the router container's terminal, then copy that snapshot to protected storage. To use the helper below on the server, run it from the deployed Compose directory with the same Compose project name and environment that Dokploy uses (set `COMPOSE_PROJECT_NAME` if necessary).

```bash
bash scripts/backup.sh
```

This uses SQLite `VACUUM INTO` for a consistent online database copy. Do not copy only the live `.sqlite` file while WAL mode is active. Store the encryption key separately; a database backup alone cannot recover Claude credentials. Treat database backups as private even though provider tokens are encrypted.

The script writes `router-<UTC timestamp>.sqlite` (mode 600) to `./backups` and prints its absolute path. On the server, set `BACKUP_DIR` to a directory outside the Dokploy checkout, because Dokploy may replace that directory on redeploy. `BACKUP_KEEP=N` keeps only the newest N `router-*.sqlite` files in that directory; without it, nothing is pruned. When `sqlite3` is installed on the host, the script runs `PRAGMA integrity_check` on the copy; a failed check keeps the file, skips pruning, and exits nonzero. Without `sqlite3` it warns and skips the check. The temporary snapshot inside `/data` is removed on every exit, including failures and Ctrl-C, and a partial local copy is deleted.

```bash
BACKUP_DIR=/srv/router-backups BACKUP_KEEP=14 bash scripts/backup.sh
```

To restore, stop the router and run `bash scripts/restore.sh BACKUP_FILE` on the Docker host. It saves the current database as `pre-restore-<timestamp>.sqlite` in `BACKUP_DIR` before replacing it; the runbook below covers the full procedure.

Database migrations run automatically at every startup and are forward-only. Take a backup before each upgrade; rolling back means redeploying the previous version and restoring that backup.

The [operations runbook](docs/operations.md) has copy-paste commands for finding the Compose volume, backing up, restoring, upgrading, rotating the encryption key or owner password, and troubleshooting `needs_reauth`, sign-in throttling, and `/readyz` failures.

## Development and verification

```bash
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

CI runs fmt and clippy on Rust 1.97, pinned through the SHA-pinned `dtolnay/rust-toolchain` step (`# 1.97`) in [`.github/workflows/ci.yml`](.github/workflows/ci.yml), which matches the `rust:1.97-bookworm` build image in the `Dockerfile`. Bump both together, so new clippy lints arrive through a deliberate change. A separate `msrv` job runs `cargo check --locked --all-targets` with Rust 1.94, the `rust-version` in `Cargo.toml`.

Tests run against temporary SQLite databases and mock HTTP upstreams. They require loopback socket access and never use your real Claude credentials. They cover model denial, admin isolation, key rotation, token arithmetic, streaming cancellation, recovery, encrypted credentials, PKCE state, refresh concurrency, and sanitized failures.

An optional compatibility test runs the real opencodex Anthropic adapter from the `@bitkyc08/opencodex` npm package against the mock-backed router. It needs Bun and the unpacked package, and it does not touch your own opencodex configuration. The tested version is `OPENCODEX_VERSION` in [`.github/workflows/opencodex-smoke.yml`](.github/workflows/opencodex-smoke.yml), which runs the same test weekly and can be started manually from the Actions tab. To run it locally against that version:

```bash
opencodex_version=$(sed -n 's/^ *OPENCODEX_VERSION: "\(.*\)"$/\1/p' .github/workflows/opencodex-smoke.yml)
npm install --prefix /tmp/opencodex --ignore-scripts "@bitkyc08/opencodex@$opencodex_version"
OPENCODEX_SOURCE=/tmp/opencodex/node_modules/@bitkyc08/opencodex \
cargo test --locked opencodex_adapter_smoke -- --ignored
```

Production readiness still requires completing browser OAuth and one live allowed-model streaming/tool request on your deployed server. The offline suite proves the router contract, not Anthropic’s current account entitlement or OAuth availability.

See [CONTRIBUTING.md](CONTRIBUTING.md) for the commit convention and the invariants every change must keep.

## Licenses

Shared Router is MIT licensed. [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) covers bundled SQLite and adapted opencodex code, and [THIRD_PARTY_NOTICES_CRATES.md](THIRD_PARTY_NOTICES_CRATES.md) lists every Rust crate compiled into the Linux binary with its license text. Include both files when you distribute the binary. The Docker image already carries them, with `LICENSE`, in `/usr/share/doc/shared-router/`.

Regenerate the crate list whenever `Cargo.lock` changes:

```bash
cargo install --locked cargo-about
cargo about generate --locked about.hbs -o THIRD_PARTY_NOTICES_CRATES.md
```

`about.toml` lists the accepted licenses. A new dependency under any other license makes generation fail until the license is reviewed and added there.
