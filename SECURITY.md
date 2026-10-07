# Security policy

Shared Router holds a Claude account owner's OAuth credentials and decides which router keys may reach which models. A flaw here can expose that account, so please report vulnerabilities privately.

## Reporting a vulnerability

Use GitHub private vulnerability reporting: open the [private advisory form](https://github.com/SobshDev/shared_router/security/advisories/new) for this repository. Do not open a public issue, pull request, or discussion for a suspected vulnerability.

Please include the affected commit or tag, the deployment setup (Dokploy, local Compose, or native binary), reproduction steps, and the impact you observed. Redact real router keys, OAuth tokens, session cookies, encryption keys, and password hashes from anything you send; a placeholder is enough to show where a secret appears.

## Scope

These reports are in scope:

- Reaching a model through the router that the key was not granted, or that is not reviewed and enabled, with any endpoint, alias, or request shape.
- Bypassing router key authentication, per-key model grants, or key revocation.
- Bypassing owner dashboard authentication, the session cookie, the exact-origin check, or the CSRF token.
- Leaking the owner's Claude OAuth tokens, the `ENCRYPTION_KEY`, the admin password or its hash, router key secrets, or session tokens.
- Prompts, completions, credentials, or raw provider error bodies appearing in logs, in the database, or in responses to other callers.
- Causing the router to replay or retry a `/v1/messages` request upstream.
- Reaching upstream endpoints, request fields, beta features, or server tool types that the router does not allowlist.

These are out of scope: vulnerabilities in Anthropic's services or in opencodex, attacks that need control of the host, the Docker daemon, the database volume, or the Dokploy account, and denial of service or heavy usage by an authorized friend key: the router does not limit how many requests a key runs.

## Supported versions

Only the latest tagged release and the current `main` branch receive security fixes. Upgrade to the latest release before reporting if you can.

## Response

This is a single-maintainer project, so responses are best effort. Expect an acknowledgement within 7 days and an initial assessment within 14 days. Fixes are released as a new tag with a `CHANGELOG.md` entry, and the advisory is published once a fixed release is available. Reporters are credited in the advisory unless they ask otherwise.
