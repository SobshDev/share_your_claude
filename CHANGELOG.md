# Changelog

All notable changes to Shared Router are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html). While the version is below 1.0, a minor bump (0.x.0) marks a release that needs operator action, such as new migrations or configuration changes. Patch releases need none.

Entries marked **Breaking** require an action when upgrading; follow the steps listed with them. See the [operations runbook](docs/operations.md) for backing up before an upgrade and rolling back.

## [Unreleased]

### Added

- `SECURITY.md` with a private vulnerability reporting path.
- An operations runbook (`docs/operations.md`) covering backup, restore, upgrades, key and password rotation, and troubleshooting.
- An architecture overview (`docs/architecture.md`) and the accepted request surface in the README.
- `CONTRIBUTING.md` and a pull request template.
- `cargo-about` configuration for generating third-party license notices.

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
