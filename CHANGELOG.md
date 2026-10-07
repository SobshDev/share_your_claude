# Changelog

All notable changes to Shared Router are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html). While the version is below 1.0, a minor bump (0.x.0) marks a release that needs operator action, such as new migrations or configuration changes. Patch releases need none.

Entries marked **Breaking** require an action when upgrading; follow the steps listed with them. See the [operations runbook](docs/operations.md) for backing up before an upgrade and rolling back.

## [Unreleased]

This release needs operator action: it adds four migrations, a new setting for deployments behind a proxy, and changes to the CLI and the Docker image. Take a backup before upgrading.

### Upgrade steps

1. Back up the database (`bash scripts/backup.sh`, see the [operations runbook](docs/operations.md)).
2. Behind Dokploy's Traefik, set `TRUSTED_PROXY_HOPS=1` in the Environment settings, so sign-in throttling can tell clients apart. The value must be a non-negative integer; anything else stops the router at startup.
3. Keep `stop_grace_period` at 30 seconds or more if you run your own Compose file; shutdown now takes up to 28 seconds.
4. Redeploy. Migrations `0003` to `0006` run automatically at startup. `0003` signs every dashboard session out once, so sign in again afterwards.
5. Fable 5.1 can now be granted like any other model. It stays off after the upgrade; switch it on in **Models** and grant it per key only if you want friends to use it.

### Security

- Sign-in throttling counts failures per client address: five per minute for each address and 30 per minute in total, and a successful sign-in clears the caller's failures. A stranger can no longer lock the owner out by sending wrong passwords. The client address comes from `X-Forwarded-For` when the new `TRUSTED_PROXY_HOPS` setting is set (#1).
- An upstream 401 no longer disconnects Claude for everyone. The router refreshes the token once, resends token counts, and answers `/v1/messages` with a retryable 503 instead of replaying it. **Refresh from Claude** in the dashboard also refreshes once and fetches the catalog page again. The connection needs reconnecting only when the refresh is rejected, the refreshed token is refused again, or a 403 is an `authentication_error`; other 403s return `permission_error` (#2).
- Friend routes authenticate the router key from the headers before reading the request body, so unauthenticated requests cannot make the router buffer and parse up to 32 MiB. Other routes are limited to 64 KiB (#3).
- Dashboard sessions are bound to the `ADMIN_PASSWORD_HASH` they were issued under, so changing it signs every session out (#6).
- A stored Claude credential that cannot be decrypted, for example after an `ENCRYPTION_KEY` change, now marks the connection as needing reconnection and logs a fixed message, instead of failing every request with 500 while the dashboard shows it connected (#7).
- With an HTTPS `PUBLIC_ORIGIN`, every response carries `Strict-Transport-Security: max-age=31536000` (#54).
- The database, its directory, and backups are created owner-only (files 600, directories 700) (#53). PKCE verifiers, authorization codes, token request bodies, and newly issued key secrets are wiped from memory after use (#46).

### Fable 5.1

- **Breaking:** Fable 5.1 is no longer blocked. The owner can switch it on, grant it per key, alias it, and list it in `/v1/models` like any other reviewed model. Migration `0005_forbid_fable_grants.sql` still removes Fable grants from earlier versions, and `0006_allow_fable.sql` then drops its triggers and moves Fable back into the `claude` group, so every key starts without Fable (#4, #5, #55).
- `POST /admin/api/keys` accepts an optional `models` list; without it a new key gets every enabled model.

### Added

- `shared-router healthcheck` requests `/readyz` on the configured port, and `shared-router help` (also `-h`, `--help`) prints usage (#57).
- `TRUSTED_PROXY_HOPS` setting, forwarded by `compose.yaml` (default 0). With N greater than 0 the client address is the N-th `X-Forwarded-For` entry from the right; an invalid value stops startup (#1).
- `scripts/restore.sh` replaces the database from a backup after saving the current one (#36).
- `scripts/backup.sh` accepts `BACKUP_DIR` and `BACKUP_KEEP=N`, checks the copy with `sqlite3` when installed, and always removes its temporary snapshot from the data volume (#37).
- `compose.local.yaml` publishes the router on `127.0.0.1:8080` for local testing (#40).
- The Docker image ships `LICENSE` and the third-party notices in `/usr/share/doc/shared-router/` (#70).
- `SECURITY.md` with a private vulnerability reporting path (#42).
- An operations runbook (`docs/operations.md`) covering backup, restore, upgrades, key and password rotation, and troubleshooting (#36).
- An architecture overview (`docs/architecture.md`), the accepted request surface, a configuration reference, and a command reference in the README (#44, #69).
- `CONTRIBUTING.md` and a pull request template (#72).
- `cargo-about` configuration and a generated list of every bundled crate license (#70).
- The `effort-2025-11-24` and `mid-conversation-system-2026-04-07` betas are accepted, so Claude Code can use the router directly with `ANTHROPIC_BASE_URL` and `ANTHROPIC_API_KEY`.
- The `structured-outputs-2025-11-13` beta and the `eager_input_streaming` tool field are accepted, so opencode can use the router through its `@ai-sdk/anthropic` provider.

### Changed

- The caller's `system` prompt is sent to Claude at the start of the first user message, wrapped in `<system-reminder>`, instead of in `system`, which now holds only the router's identity block. Claude rejected opencode's system prompt with a 400 in `system` over the owner's subscription login, and accepts it in this position, as Claude Code sends it.
- The dashboard is redesigned: a side rail with Overview, Requests, People & keys, Models, and Claude connection; one shared filter bar for person, model, and period; a daily chart with share and token mix breakdowns; key rows with model chips and a menu for model access, opencodex setup, and revocation; model switches; and a two-step Claude connection. A new key's sheet shows the secret once inside a ready opencodex configuration. The interface is white with an evergreen accent and uses Manrope and IBM Plex Mono when installed, falling back to system fonts.
- **Breaking:** `shared-router hash-password` reads only piped input and refuses a terminal, where the password would be echoed. Commands with extra arguments now print usage and exit with status 2 instead of ignoring them (#57).
- **Breaking:** the Docker image no longer contains `curl`; its health check runs `shared-router healthcheck`. Replace any `docker exec … curl` with `docker exec … shared-router healthcheck` (#68).
- **Breaking:** migration `0003_admin_session_password.sql` deletes all existing dashboard sessions once (#6).
- On SIGTERM the router reports `/readyz` as 503 and drains open requests for up to 20 seconds. Open streams then save their usage, are recorded as `interrupted`, and end; after 3 more seconds any request still open is recorded as `interrupted` with the shutdown time, and the database is closed within 5 seconds. Rows recovered after a crash keep an empty `finished_at` (#11).
- The 30-minute limit on every upstream call is replaced by per-request limits: streams may run for 60 minutes and then end with a `timeout_error` event and an `interrupted` record, non-streaming `/v1/messages` calls time out after 600 seconds, token counts after 60 seconds, and each catalog page after 30 seconds (#12).
- Dashboard assets are linked as `/assets/…?v=<hash>` and cached for a year; unversioned asset requests get `no-cache`, and pages and API responses stay `no-store` (#54).
- Error responses use Anthropic's error types and JSON envelope, including unmatched routes and body-limit rejections. Rejected query strings say "Invalid query parameters", other malformed requests "Invalid request", and a non-streaming reply from an unexpected model "Claude answered with a different model than requested". Dashboard and sign-in pages render a small HTML 500 page when they cannot be displayed (#9).
- Configuration errors name the variable at fault (#26).
- Logs record a fixed category, the router request id, and the upstream status for each failed gateway request, and only the kind and result code of database errors, never their messages (#13).
- The OAuth refresh no longer blocks requests whose token is still valid, backs off for 30 seconds after a transient failure, and keeps a rotated token in memory if saving it fails (#45).
- Key `last_used_at` is written at most once a minute and never fails a request (#48).
- `anthropic-beta` values are combined across header lines and deduplicated; the caller allowlist is `proxy::CLIENT_BETAS` (#49).
- Migration `0004_usage_model_index_and_label_check.sql` replaces the model usage index with one on the effective model and limits key labels to 1–100 characters (#55).
- `compose.yaml` rotates logs at 10 MB × 5 files and limits the container to 512 MB of memory and 256 processes (#38).
- Dependencies moved to askama 0.16, reqwest 0.13, sqlx 0.9, argon2 0.6, and chacha20poly1305 0.11. Outbound TLS still uses rustls with ring and the Mozilla root set. Stored Claude credentials and existing owner password hashes keep working; a fixture test pins both. The minimum supported Rust version is now 1.94, which sqlx 0.9 requires (#67).
- The Docker build caches dependencies between builds, and the runtime image adds OCI labels (#68).
- CI runs with read-only token permissions and pinned actions, pins the lint toolchain to Rust 1.97 alongside the Dockerfile, checks the MSRV, builds and smoke-tests the Docker image, audits dependencies with `cargo-deny`, and checks dashboard JavaScript syntax. Dependabot tracks Cargo, GitHub Actions, and Docker base images. A manual workflow runs the opencodex smoke test against a pinned version (#31, #32, #33, #34, #35, #63).

### Fixed

- Null usage counters are treated as unreported instead of invalid (#10).
- A client that stops reading a stream is recorded as `interrupted`, separately from upstream errors (#50).
- The SSE decoder is incremental and handles the whole event-stream format (#52).
- Empty analytics filters are treated as absent (#56).
- Saving **Edit access** keeps grants for disabled models, and the form shows them (#16).
- Dashboard: a double-click on Revoke no longer revokes a key without confirmation (#15); sign-in shows clear messages for wrong passwords, network, and parse failures (#17); re-renders keep keyboard focus and scroll position (#18); live regions, tables, and grant controls are accessible to screen readers (#19); the connection status refreshes after reauthentication errors (#60); an uncopied API key stays open on Escape and is selected when copying fails (#61).
- Rebuilds pick up changed migrations (#14).
- `scripts/bootstrap-secrets.sh` checks for the local image before asking for a password and runs its containers without network access (#41).

## [0.1.0] - 2026-09-15

First release, covering the initial gateway and three follow-up changes.

### Added

- Private Claude gateway with per-person router keys, per-key model grants, a reviewed model catalog with aliases, and Fable 5.1 blocked for every router key (67fd4ce).
- Anthropic-compatible `/v1/messages` (JSON and SSE), `/v1/messages/count_tokens`, and `/v1/models` for friend keys (67fd4ce).
- Owner dashboard with Argon2id sign-in, session cookies, exact-origin and CSRF checks, and Claude OAuth PKCE login with encrypted credential storage and coordinated refresh (67fd4ce).
- Per-request usage accounting in SQLite with streaming checkpoints and startup recovery, plus `backup`, `generate-key`, and `hash-password` commands (67fd4ce).
- Usage analytics dashboard and `/admin/api/analytics`: daily token, request, and error charts, person and model share, a grouped ledger, and paginated request history. Migration `0002_usage_time.sql` adds an index and runs automatically at startup (d9e3dad).

### Changed

- **Breaking:** secrets are read from direct environment values (69cc644). The router now reads only `ENCRYPTION_KEY` and `ADMIN_PASSWORD_HASH` as values; `ENCRYPTION_KEY_FILE` and `ADMIN_PASSWORD_HASH_FILE` are no longer read, and `compose.yaml` no longer uses `ENCRYPTION_KEY_SOURCE`, `ADMIN_PASSWORD_HASH_SOURCE`, or Compose secrets. To upgrade:
  1. Copy the contents of your existing encryption key file into `ENCRYPTION_KEY`, and the contents of your password hash file into `ADMIN_PASSWORD_HASH` (single-quoted). Reuse the existing key; generating a new one makes the saved Claude login unreadable.
  2. Remove the old `_SOURCE` and `_FILE` settings and any Dokploy File Mounts for them.
  3. Redeploy.

### Fixed

- The Claude OAuth flow uses the registered `http://localhost:54545/callback` redirect URI; `127.0.0.1` is rejected by the OAuth client (2e7346a).

[Unreleased]: https://github.com/SobshDev/shared_router/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/SobshDev/shared_router/releases/tag/v0.1.0
