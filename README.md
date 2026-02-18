# Temper

[![CI](https://github.com/nerdsane/temper/actions/workflows/ci.yml/badge.svg)](https://github.com/nerdsane/temper/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue)](LICENSE)
[![Rust](https://img.shields.io/endpoint?url=https://gist.githubusercontent.com/rita-aga/883fd73429759b545967fdd6298b34ff/raw/temper-rust.json)](https://www.rust-lang.org)
[![Tests](https://img.shields.io/endpoint?url=https://gist.githubusercontent.com/rita-aga/883fd73429759b545967fdd6298b34ff/raw/temper-tests.json)](#)
[![Crates](https://img.shields.io/endpoint?url=https://gist.githubusercontent.com/rita-aga/883fd73429759b545967fdd6298b34ff/raw/temper-crates.json)](#)

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

## Running

```bash
cargo test --workspace

DATABASE_URL=postgres://user:pass@localhost/db cargo run -- serve \
  --specs-dir reference-apps/ecommerce/specs --tenant ecommerce

./scripts/bench.sh
```

## Documentation

- [docs/PAPER.md](docs/PAPER.md) -- Research paper
- [docs/POSITIONING.md](docs/POSITIONING.md) -- The observation that motivated this work
- [docs/AGENT_GUIDE.md](docs/AGENT_GUIDE.md) -- Technical reference

## Status

594 tests across 18 crates. Functional end-to-end: spec parsing, verification cascade, actor dispatch, Postgres persistence, OTEL telemetry. The open questions are about generality, not functionality.
