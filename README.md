# Temper

[![CI](https://github.com/nerdsane/temper/actions/workflows/ci.yml/badge.svg)](https://github.com/nerdsane/temper/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue)](LICENSE)
[![Rust](https://img.shields.io/endpoint?url=https://gist.githubusercontent.com/rita-aga/883fd73429759b545967fdd6298b34ff/raw/temper-rust.json)](https://www.rust-lang.org)
[![Tests](https://img.shields.io/endpoint?url=https://gist.githubusercontent.com/rita-aga/883fd73429759b545967fdd6298b34ff/raw/temper-tests.json)](#)
[![Crates](https://img.shields.io/endpoint?url=https://gist.githubusercontent.com/rita-aga/883fd73429759b545967fdd6298b34ff/raw/temper-crates.json)](#)
[![MCP](https://img.shields.io/badge/MCP-Code%20Mode-0A84FF)](#integrations)
[![OpenClaw](https://img.shields.io/badge/OpenClaw-Plugin-0EA5E9)](#integrations)

**This is research, not a product.**

Temper explores a hypothesis: most enterprise SaaS backends are state machines at their core -- an order moves through Draft, Submitted, Shipped, Delivered; a subscription cycles between Active, PastDue, Cancelled. If the state machine is the essential artifact, the surrounding infrastructure (persistence, API, authorization, webhooks, observability) follows mechanically from the specification.

The question is how far this can be pushed. This codebase is an attempt to find out.

---

## Overview

An actor-based framework where I/O Automaton specifications define entity behavior, a four-level verification cascade validates correctness before deployment, and a conversational platform generates specifications from developer interviews.

- Specifications are declarative: states, transitions, guards, invariants, integrations
- Verification is automated: SMT symbolic checking, exhaustive model checking, deterministic simulation, property-based testing
- The HTTP API is derived from the data model -- agents can discover it through a metadata endpoint
- Production usage feeds back through an evolution engine that captures unmet user intents

A reference e-commerce application exercises the full stack: three entity types (Order, Payment, Shipment) verified through the cascade, persisted to Postgres, traced to ClickHouse. Four additional fixture specs (support ticket, approval workflow, subscription management, issue tracker) test the pattern across domains.

## Scope

This approach works for applications whose core logic is state machine shaped. That covers a meaningful subset of enterprise SaaS, but not all backend systems. The state model is a finite automaton (status + counters + booleans) -- no floating-point, no strings, no cross-entity invariants. Some of these are fundamental to the approach; others are engineering work not yet done.

| Limitation | Status |
|-----|-------|
| Single-node only | Redis traits designed, not wired |
| No cross-entity invariants | Integration engine orchestrates |
| No temporal guards | Planned via integration engine |
| Spec generation requires an LLM | Specs are also hand-writable |
| No UI layer | OData API; any frontend works |

[docs/POSITIONING.md](docs/POSITIONING.md) has a fuller discussion.

## Getting Started

Run tests, then choose a storage backend explicitly:

```bash
cargo test --workspace

# Postgres (production / multi-tenant default)
DATABASE_URL=postgres://user:pass@localhost/db cargo run -- serve \
  --storage postgres \
  --specs-dir reference-apps/ecommerce/specs --tenant ecommerce

# Turso/libSQL (edge + low-ops; local file mode needs no cloud account)
TURSO_URL=file:local.db cargo run -- serve \
  --storage turso \
  --specs-dir reference-apps/ecommerce/specs --tenant ecommerce

# Redis (ephemeral/cache-oriented workflows)
REDIS_URL=redis://127.0.0.1:6379 cargo run -- serve \
  --storage redis \
  --specs-dir reference-apps/ecommerce/specs --tenant ecommerce
```

## Running

```bash
cargo test --workspace

DATABASE_URL=postgres://user:pass@localhost/db cargo run -- serve \
  --storage postgres \
  --specs-dir reference-apps/ecommerce/specs --tenant ecommerce

./scripts/bench.sh
```

## Storage Backends

Temper supports three event-store backends selected with `--storage postgres|turso|redis`:

| Backend | Required env | Optional env | Best for |
|-----|-----|-----|-----|
| Postgres | `DATABASE_URL` | - | Production, multi-tenant workloads, existing infra |
| Turso/libSQL | `TURSO_URL` | `TURSO_AUTH_TOKEN` (optional for local `file:`) | Edge deployment, embedded/local DBs, low-ops |
| Redis | `REDIS_URL` | - | Ephemeral/cache use cases |

Local Turso dev example:

```bash
TURSO_URL=file:local.db cargo run -- serve --storage turso \
  --specs-dir reference-apps/ecommerce/specs --tenant ecommerce
```

## Integrations

### MCP (Code Mode)

Temper exposes `search` and `execute` tools over stdio MCP:

```bash
cargo run -- mcp --app my-app=path/to/specs --port 3001
```

- Pattern: https://blog.cloudflare.com/code-mode-mcp/
- Sandbox runtime: https://github.com/pydantic/monty

### OpenClaw Plugin

The repo includes `plugins/openclaw-temper`, which adds:
- `temper` tool (`list`, `get`, `create`, `action`, `patch`)
- Background SSE subscriber that wakes OpenClaw agents from `/tdata/$events`

Install and configure:

```bash
openclaw plugins install ./plugins/openclaw-temper
```

Add plugin config under `plugins.entries.temper.config` in `~/.openclaw/openclaw.json` (URL, hooks token/port, app-to-agent routing map).

## Documentation

- [docs/PAPER.md](docs/PAPER.md) -- Research paper
- [docs/POSITIONING.md](docs/POSITIONING.md) -- The observation that motivated this work
- [docs/AGENT_GUIDE.md](docs/AGENT_GUIDE.md) -- Technical reference

## Status

594 tests across 18 crates. Functional end-to-end: spec parsing, verification cascade, actor dispatch, Postgres persistence, OTEL telemetry. The open questions are about generality, not functionality.
