## Summary

<!-- What changes, and why. Link issues with "Closes #N". -->

## Checklist

- [ ] `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, and `cargo test --locked` pass
- [ ] `node --check static/*.js` passes (if dashboard JavaScript changed)
- [ ] No secrets, tokens, keys, passwords, prompts, completions, or raw provider bodies are logged or stored
- [ ] Keys still reach only their granted, reviewed, enabled models on every endpoint
- [ ] No `/v1/messages` request can be replayed or retried upstream
- [ ] README API tables and accepted request surface updated (if routes, fields, betas, or tool types changed)
- [ ] New migration added (if the schema changed); existing migrations untouched
- [ ] `CHANGELOG.md` updated under **Unreleased** (breaking changes include upgrade steps)
- [ ] Third-party notices regenerated (if `Cargo.lock` changed)
