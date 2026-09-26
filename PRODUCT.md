# Shared Router

<!-- impeccable:product-schema 1 -->

## Platform
web

## Stack
Rust/Axum, SQLite, Askama with small JavaScript enhancements, and Docker Compose on Dokploy. Dokploy manages domain routing and HTTPS.

## Users
One Claude account owner manages access for two or three friends. Friends use local opencodex and never sign into this dashboard.

## Product Purpose
Issue individual keys, keep Fable 5.1 unreachable through the router for every key, and understand each person's observed token consumption.

## Operating Context
An owner-only dashboard on a single HTTPS server. Key setup is occasional; usage inspection and connection health are recurring tasks.

## Capabilities and Constraints
People, multiple keys per person, model grants, Claude OAuth login, streaming usage accounting, and a usage analytics dashboard. Reporting only, no token budgets. Partial/unknown measurements must be visible. No prompts, responses, or upstream secrets displayed or logged. A new key starts with every model enabled at creation time except Fable 5.1; later catalog changes never alter existing keys' grants.

## Product Principles
Make access explicit. Preserve attribution across key rotation. Distinguish unknown usage from zero. Keep subscription credentials on the server.

## Evidence on Hand
The locally installed opencodex 2.49.0 adapter source, and real per-request usage recorded in SQLite and shown in the usage analytics dashboard (daily charts, person/model/key breakdowns, and request history).
