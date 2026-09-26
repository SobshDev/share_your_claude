# Changelog

All notable changes to Shared Router are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html). While the version is below 1.0, a minor bump (0.x.0) marks a release that needs operator action, such as new migrations or configuration changes. Patch releases need none.

Entries marked **Breaking** require an action when upgrading; follow the steps listed with them. See the [operations runbook](docs/operations.md) for backing up before an upgrade and rolling back.

## [Unreleased]

This release needs operator action: it adds three migrations, a new setting for deployments behind a proxy, and changes to the CLI and the Docker image. Take a backup before upgrading.

### Upgrade steps

1. Back up the database (`bash scripts/backup.sh`, see the [operations runbook](docs/operations.md)).
2. Behind Dokploy's Traefik, set `TRUSTED_PROXY_HOPS=1` in the Environment settings, so sign-in throttling can tell clients apart.
3. Redeploy. Migrations `0003` to `0005` run automatically at startup. `0003` signs every dashboard session out once, so sign in again afterwards.

### Security

- Sign-in throttling counts failures per client address: five per minute for each address and 30 per minute in total, and a successful sign-in clears the caller's failures. A stranger can no longer lock the owner out by sending wrong passwords. The client address comes from `X-Forwarded-For` when the new `TRUSTED_PROXY_HOPS` setting is set (#1).
- An upstream 401 no longer disconnects Claude for everyone. The router refreshes the token once, resends token counts, and answers `/v1/messages` with a retryable 503 instead of replaying it. The connection needs reconnecting only when the refresh is rejected or a 403 is an `authentication_error`; other 403s return `permission_error` (#2).
- Friend routes authenticate the router key from the headers before reading the request body, so unauthenticated requests cannot make the router buffer and parse up to 32 MiB. Other routes are limited to 64 KiB (#3).
- Dashboard sessions are bound to the `ADMIN_PASSWORD_HASH` they were issued under, so changing it signs every session out (#6).
- A stored Claude credential that cannot be decrypted, for example after an `ENCRYPTION_KEY` change, now marks the connection as needing reconnection and logs a fixed message, instead of failing every request with 500 while the dashboard shows it connected (#7).
- Admission control is per key as well as global: each key may run 3 `/v1/messages` requests at once, all keys together 8, and token counts use a separate pool of 4. Busy responses are `429 rate_limit_error` with `retry-after: 1` (#8).
- The database, its directory, and backups are created owner-only (files 600, directories 700) (#53). PKCE verifiers, authorization codes, token request bodies, and newly issued key secrets are wiped from memory after use (#46).

### Fable 5.1 hardening

- `policy::blocked` normalizes case and separators, so spellings such as `claude-fable-5.1` and `CLAUDE-FABLE-5-1` are blocked, and every layer, including key creation and catalog refresh, uses it (#4).
- Migration `0005_forbid_fable_grants.sql` deletes any existing Fable 5.1 grants and adds triggers that refuse Fable grants in the database itself and revoke grants when a model moves into the Fable group (#55).
- Tests cover every block layer on every endpoint, including a stream whose reply reports Fable (#5).

### Added

- `shared-router healthcheck` requests `/readyz` on the configured port, and `shared-router help` (also `-h`, `--help`) prints usage (#57).
- `TRUSTED_PROXY_HOPS` setting, forwarded by `compose.yaml` (default 0) (#1).
- `scripts/restore.sh` replaces the database from a backup after saving the current one (#36).
- `scripts/backup.sh` accepts `BACKUP_DIR` and `BACKUP_KEEP=N`, checks the copy with `sqlite3` when installed, and always removes its temporary snapshot from the data volume (#37).
- `compose.local.yaml` publishes the router on `127.0.0.1:8080` for local testing (#40).
- The Docker image ships `LICENSE` and the third-party notices in `/usr/share/doc/shared-router/` (#70).
- `SECURITY.md` with a private vulnerability reporting path (#42).
- An operations runbook (`docs/operations.md`) covering backup, restore, upgrades, key and password rotation, and troubleshooting (#36).
- An architecture overview (`docs/architecture.md`), the accepted request surface, a configuration reference, and a command reference in the README (#44, #69).
- `CONTRIBUTING.md` and a pull request template (#72).
- `cargo-about` configuration and a generated list of every bundled crate license (#70).

### Changed

- **Breaking:** `shared-router hash-password` reads only piped input and refuses a terminal, where the password would be echoed. Commands with extra arguments now print usage and exit with status 2 instead of ignoring them (#57).
- **Breaking:** the Docker image no longer contains `curl`; its health check runs `shared-router healthcheck`. Replace any `docker exec … curl` with `docker exec … shared-router healthcheck` (#68).
- **Breaking:** migration `0003_admin_session_password.sql` deletes all existing dashboard sessions once (#6).
- On SIGTERM the router reports `/readyz` as 503, drains open requests for up to 20 seconds, records the rest as `interrupted` with the shutdown time, and closes the database. Rows recovered after a crash keep an empty `finished_at` (#11).
- Error responses use Anthropic's error types and JSON envelope, including unmatched routes and body-limit rejections (#9).
- The OAuth refresh no longer blocks requests whose token is still valid, backs off for 30 seconds after a transient failure, and keeps a rotated token in memory if saving it fails (#45).
- Key `last_used_at` is written at most once a minute and never fails a request (#48).
- `anthropic-beta` values are combined across header lines and deduplicated; the caller allowlist is `proxy::CLIENT_BETAS` (#49).
- Migration `0004_usage_model_index_and_label_check.sql` replaces the model usage index with one on the effective model and limits key labels to 1–100 characters (#55).
- `compose.yaml` rotates logs at 10 MB × 5 files and limits the container to 512 MB of memory and 256 processes (#38).
- The Docker build caches dependencies between builds, and the runtime image adds OCI labels (#68).
- CI runs with read-only token permissions and pinned actions, pins the lint toolchain to Rust 1.97 alongside the Dockerfile, checks the 1.88 MSRV, builds and smoke-tests the Docker image, audits dependencies with `cargo-deny`, and checks dashboard JavaScript syntax. Dependabot tracks Cargo, GitHub Actions, and Docker base images. A manual workflow runs the opencodex smoke test against a pinned version (#31, #32, #33, #34, #35, #63).

### Fixed

- Null usage counters are treated as unreported instead of invalid (#10).
- A client that stops reading a stream is recorded as `interrupted`, separately from upstream errors (#50).
- The SSE decoder is incremental and handles the whole event-stream format (#52).
- Empty analytics filters are treated as absent (#56).
- Saving **Edit access** keeps grants for disabled models, and the form shows them (#16).
- Dashboard: a double-click on Revoke no longer revokes a key without confirmation (#15); sign-in shows clear messages for wrong passwords, network, and parse failures (#17); re-renders keep keyboard focus and scroll position (#18); live regions, tables, and grant controls are accessible to screen readers (#19); the connection status refreshes after reauthentication errors (#60); an uncopied API key stays open on Escape and is selected when copying fails (#61).
- Rebuilds pick up changed migrations (#14).

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
