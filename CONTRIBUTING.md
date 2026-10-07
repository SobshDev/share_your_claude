# Contributing

Shared Router is a small, single-maintainer project that holds a Claude account owner's credentials. Changes are welcome, but every change must keep the invariants below. Report security problems privately through [SECURITY.md](SECURITY.md), never in a public issue or pull request.

## Development setup

You need Rust 1.94 or newer and a C compiler (SQLite is compiled in). [README.md](README.md) explains how to run the router locally, and [docs/architecture.md](docs/architecture.md) maps the modules and the request flow.

Run these before opening a pull request; CI runs the first three:

```bash
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
node --check static/*.js
```

The tests use temporary SQLite databases and mock upstream servers, never real Claude credentials. They need permission to open loopback sockets, so they can fail inside restrictive sandboxes.

An ignored smoke test runs the real opencodex Anthropic adapter against the mock-backed router. Run it when you change the proxy, the request allowlists, tool name mapping, or streaming, either locally or through the manual **opencodex smoke** workflow. It needs Bun and an unpacked `@bitkyc08/opencodex` package at the version pinned as `OPENCODEX_VERSION` in `.github/workflows/opencodex-smoke.yml`:

```bash
OPENCODEX_SOURCE=/absolute/path/to/node_modules/@bitkyc08/opencodex \
cargo test --locked opencodex_adapter_smoke -- --ignored
```

## Invariants

These are not negotiable. A pull request that weakens one will not be merged, even behind an option.

- **A key reaches only the models it was granted.** Grants accept only reviewed, enabled models, and `policy::resolve` checks the grant, review, and enabled state again on every request before anything is sent upstream.
- **Secrets and content are never logged or stored.** Do not log, persist, or echo router keys, OAuth tokens, the encryption key, passwords or their hashes, session or CSRF tokens, prompts, completions, raw provider error bodies, or SQL values. Only the allowlisted numeric usage fields are stored.
- **The proxy never replays or retries a `/v1/messages` request upstream,** including after 429s, timeouts, or network errors. Each accepted request is sent upstream at most once.
- **One replica per database.** Refresh coordination, the login throttle, and pending OAuth logins are process-local, and startup recovery assumes no other process is writing. Do not add behavior that depends on multiple replicas.
- **New upstream surface is reviewed explicitly.** Request fields, control keys, tool types, and `anthropic-beta` values reach Claude only through the allowlists in `policy.rs` and `proxy::CLIENT_BETAS`. Add to them deliberately, and update the accepted request surface table in the README in the same change.
- **Unknown usage stays unknown.** Missing token counts are never estimated or shown as zero.

## Commits and pull requests

Commits use [Conventional Commits](https://www.conventionalcommits.org/): `type(scope): subject`, with a lowercase imperative subject and no trailing period, for example `fix(oauth): use registered localhost redirect URI`.

- Types: `feat`, `fix`, `refactor`, `docs`, `test`, `chore`, `build`, `ci`.
- The scope is the area touched, usually a module or component such as `router`, `proxy`, `oauth`, `config`, `analytics`, or `admin`.
- Use the body to explain why the change is needed. Reference issues with a trailer line such as `Closes #12`.

Keep each pull request focused on one change. When relevant:

- Add a new migration file under `migrations/` for any schema change. Migrations run automatically at startup and are forward-only, and editing an already-applied migration breaks existing databases.
- Update the README when behavior, configuration, or the API tables change, and [docs/operations.md](docs/operations.md) when an operator procedure changes.
- Add an entry under **Unreleased** in [CHANGELOG.md](CHANGELOG.md), marking changes that need operator action as **Breaking** with upgrade steps.
- Regenerate [THIRD_PARTY_NOTICES_CRATES.md](THIRD_PARTY_NOTICES_CRATES.md) with `cargo about generate --locked about.hbs -o THIRD_PARTY_NOTICES_CRATES.md` when `Cargo.lock` changes.
