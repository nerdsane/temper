# Temper

**This is research, not a product.**

Temper is an exploration of a specific hypothesis: that most enterprise SaaS backends are state machines at their core, and if you accept that premise, the surrounding infrastructure -- persistence, API endpoints, authorization, webhooks, observability -- might be derivable from a specification rather than written by hand.

We're curious how far this idea stretches. The results so far are encouraging but the open questions are genuine.

---

## What this is

An actor-based framework and conversational platform where:

- Developers describe what they want through conversation
- The system generates I/O Automaton specifications, data models, and authorization policies
- A four-level verification cascade (SMT symbolic, exhaustive model checking, deterministic simulation, property-based testing) validates everything before deployment
- Entity actors serve a self-describing HTTP API, persisting state through event sourcing
- Production usage feeds back through an evolution engine that surfaces unmet user intents

There is a working reference e-commerce application with three verified entity types (Order, Payment, Shipment) and a pattern library of four additional verified specifications (support ticket, approval workflow, subscription management, issue tracker).

## What this is not

- **Not production-ready.** This is a research codebase. APIs will change. There are known limitations (see below).
- **Not a general-purpose backend framework.** This approach works for applications whose core logic is state machine shaped. Many applications fit this pattern; some do not.
- **Not a replacement for thinking about your domain.** The system generates specs from conversation, but the developer still needs to understand what they're building.

## Known limitations

| Gap | Notes |
|-----|-------|
| Single-node only | Redis traits designed but not wired for distribution |
| No floating-point state variables | Finite automaton by design; use event payload fields |
| No cross-entity invariants | Integration engine orchestrates across entities |
| No temporal guards | No "if idle > 30 days" style conditions yet |
| Agent dependency for spec generation | The conversational platform requires an LLM; specs are also hand-writable |
| No UI layer | API only; any frontend framework can consume OData |

See [docs/POSITIONING.md](docs/POSITIONING.md) for a fuller discussion of what works and what doesn't.

## Running it

```bash
# Verify all specs (runs the four-level cascade)
cargo test --workspace

# Start the server with the reference e-commerce app
DATABASE_URL=postgres://user:pass@localhost/db cargo run -- serve \
  --specs-dir reference-apps/ecommerce/specs --tenant ecommerce

# Run benchmarks
./scripts/bench.sh
DATABASE_URL=postgres://... cargo bench -p ecommerce-reference --bench agent_checkout
```

## Reading more

- [docs/PAPER.md](docs/PAPER.md) -- Research paper with architecture, verification cascade, and benchmark results
- [docs/POSITIONING.md](docs/POSITIONING.md) -- The observation that motivated this work
- [docs/AGENT_GUIDE.md](docs/AGENT_GUIDE.md) -- Technical reference for agents building with Temper

## Status

441 tests passing across 16 crates. The system is functional end-to-end -- from specification parsing through verification, actor dispatch, Postgres persistence, and OTEL telemetry export. Whether the approach generalizes beyond the patterns we've tested remains an open question.
