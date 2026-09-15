# Shared Router

<!-- impeccable:product-schema 1 -->

## Platform
web

## Stack
Rust/Axum, SQLite, Askama with small JavaScript enhancements, and Docker Compose on Dokploy. Dokploy manages domain routing and HTTPS.

## Users
One Claude account owner manages access for two or three friends. Friends use local opencodex and never sign into this dashboard.

## Product Purpose
Issue individual keys, keep Fable 5.1 unavailable to friends, and understand each person's observed token consumption.

## Operating Context
An owner-only dashboard on a single HTTPS server. Key setup is occasional; usage inspection and connection health are recurring tasks.

## Capabilities and Constraints
People, multiple keys per person, model grants, Claude OAuth login, streaming usage accounting. Reporting only, no token budgets. Partial/unknown measurements must be visible. No prompts, responses, or upstream secrets displayed or logged.

## Product Principles
Make access explicit. Preserve attribution across key rotation. Distinguish unknown usage from zero. Keep subscription credentials on the server.

## Evidence on Hand
User-approved implementation plan and locally installed opencodex 2.49.0 adapter source. No real usage data yet.
