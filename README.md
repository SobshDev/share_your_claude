# Shared Router

A private Claude gateway for a small group. Friends use individual router keys in opencodex; the owner’s Claude OAuth credentials remain on the server. Fable 5.1 is blocked for every router key and cannot be granted; the owner reaches it only through direct Claude access, outside this router.

The Rust service includes an owner dashboard, model grants, streaming and non-streaming Messages API support, token counting, model discovery, encrypted OAuth credentials, coordinated refresh, and SQLite usage reports. It does **not** impose token budgets or convert token counts into subscription-limit percentages.

## Run locally

Requires Rust 1.88+ and a C compiler (SQLite is bundled). HTTP is accepted only for a loopback `PUBLIC_ORIGIN`; use HTTPS elsewhere.

```bash
cargo build --locked
mkdir -p data
chmod 700 data
export ENCRYPTION_KEY="$(./target/debug/shared-router generate-key)"
```

Create the owner password hash without putting the password in shell history or process arguments (Bash):

```bash
read -r -s -p 'Owner password: ' router_password; echo
export ADMIN_PASSWORD_HASH="$(printf '%s' "$router_password" | ./target/debug/shared-router hash-password)"
unset router_password

PUBLIC_ORIGIN=http://localhost:8080 ./target/debug/shared-router
```

Open http://localhost:8080/admin. Keep `PUBLIC_ORIGIN` identical to the browser origin, including its port. The default bind address is `127.0.0.1:8080`; the database defaults to `data/router.sqlite`.

## Deploy on Dokploy

Create a **Docker Compose** service in Dokploy, connect this repository, and select `compose.yaml`. Use Compose mode rather than Docker Stack, since this file builds the included Dockerfile.

1. Set `PUBLIC_ORIGIN=https://router.example.com` in the service's Environment settings, using your actual domain. This exact browser origin is used for secure cookies and CSRF checks.
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

The router runs as UID 10001 with a read-only root filesystem and a persistent `router_data` volume at `/data`. Run **one router process/replica per database**: refresh coordination and admission control are process-local, and startup recovers unfinished requests. Preserve this volume across redeployments.

For local Compose use, copy `.env.example` to `.env` and fill in the same direct values. Preserve single quotes around the password hash in `.env`; when exporting it in a shell, quote it there as well.

`GET /healthz` checks the process; `GET /readyz` checks SQLite. Readiness does not require an active Claude login, so initial setup can be completed through the dashboard.

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

No batch, arbitrary forward-proxy, Files, Managed Agents, or provider-management routes are exposed to friends. Unreviewed request fields, beta headers, server tool types, and fallback/advisor routing are rejected. Custom client tools, images, thinking, and cache controls are supported. Incoming credentials are replaced with the owner’s upstream token. Inference requests are never automatically replayed, including after 429 or network errors.

At most 8 upstream requests run at once across all keys. The router does not queue: a request beyond that limit is answered immediately with `429 rate_limit_error` ("The router is busy"), and a streaming response holds its slot until the stream ends. See [docs/architecture.md](docs/architecture.md) for the module map and the full request flow.

### Accepted request surface

Anything outside these lists is rejected with a 400 `invalid_request_error`. The allowlists live in [`src/policy.rs`](src/policy.rs) (`validate`) and [`src/proxy.rs`](src/proxy.rs) (`forward`), which are the source of truth.

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
| `anthropic-beta` header values | `claude-code-20250219`, `oauth-2025-04-20`, `prompt-caching-2024-07-31`, `interleaved-thinking-2025-05-14`, `fine-grained-tool-streaming-2025-05-14` |

`model` is required (1–200 characters) and `messages` must be an array. `/v1/messages` requires a positive integer `max_tokens`; `stream` must be a boolean, and `count_tokens` refuses `stream: true`. Tool names must be 1–128 characters and unique within a request. The contents of messages, system blocks, and tool input schemas are passed through as data. The header may list several comma-separated betas, and every one must be on the list. The router always sends `claude-code-20250219,oauth-2025-04-20` upstream and appends accepted caller betas.

The dashboard API uses session cookies, exact-origin checks, and `X-CSRF-Token` for mutations. `POST /admin/api/login` takes `{"password":"…"}`, requires the configured Origin, and returns the CSRF token; `GET /admin/api/me` returns it for an existing session. Login is limited to five attempts per minute across this single-owner service. Sessions expire after twelve hours. Friend API keys never authorize admin operations.

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

Prompts, completions, raw error bodies, API keys, and OAuth tokens are not logged or stored as usage. Only allowlisted numeric usage fields are persisted. Logs report operation failures without SQL values or provider response bodies. Do not enable HTTP body tracing or configure a reverse proxy to log credentials.

Report suspected vulnerabilities privately as described in [SECURITY.md](SECURITY.md), never in a public issue.

Terminal refresh failures mark the account as needing reconnection. Transient failures return an error without falling back to billed API access. The OAuth compatibility behavior is based on opencodex 2.49.0; live provider requirements can change. Model routing is restricted to the reviewed schema rather than passing new provider features through automatically.

## Backup and recovery

On Dokploy, you can run `shared-router backup /data/router-backup.sqlite` in the router container's terminal, then copy that snapshot to protected storage. To use the helper below on the server, run it from the deployed Compose directory with the same Compose project name and environment that Dokploy uses (set `COMPOSE_PROJECT_NAME` if necessary).

```bash
bash scripts/backup.sh
```

This uses SQLite `VACUUM INTO` for a consistent online database copy. Do not copy only the live `.sqlite` file while WAL mode is active. Store the encryption key separately; a database backup alone cannot recover Claude credentials. Treat database backups as private even though provider tokens are encrypted.

Database migrations run automatically at every startup and are forward-only. Take a backup before each upgrade; rolling back means redeploying the previous version and restoring that backup.

The [operations runbook](docs/operations.md) has copy-paste commands for finding the Compose volume, backing up, restoring, upgrading, rotating the encryption key or owner password (including invalidating existing sessions), and troubleshooting `needs_reauth`, sign-in throttling, and `/readyz` failures.

## Development and verification

```bash
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

Tests run against temporary SQLite databases and mock HTTP upstreams. They require loopback socket access and never use your real Claude credentials. They cover model denial, admin isolation, key rotation, token arithmetic, streaming cancellation, recovery, encrypted credentials, PKCE state, refresh concurrency, and sanitized failures.

An optional compatibility test runs the actual installed opencodex adapter against the mock-backed router. It needs Bun and the unpacked opencodex package, without changing your existing opencodex configuration:

```bash
OPENCODEX_SOURCE=/absolute/path/to/opencodex \
cargo test opencodex_adapter_smoke -- --ignored
```

Production readiness still requires completing browser OAuth and one live allowed-model streaming/tool request on your deployed server. The offline suite proves the router contract, not Anthropic’s current account entitlement or OAuth availability.

## Licenses

Shared Router is MIT licensed. [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) covers bundled SQLite and adapted opencodex code, and [THIRD_PARTY_NOTICES_CRATES.md](THIRD_PARTY_NOTICES_CRATES.md) lists every Rust crate compiled into the Linux binary with its license text. Include both files when you distribute the binary or a Docker image built from it.

Regenerate the crate list whenever `Cargo.lock` changes:

```bash
cargo install --locked cargo-about
cargo about generate --locked about.hbs -o THIRD_PARTY_NOTICES_CRATES.md
```

`about.toml` lists the accepted licenses. A new dependency under any other license makes generation fail until the license is reviewed and added there.
