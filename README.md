<h1 align="center">Temper</h1>

<p align="center">
  <em>Specifications are the source of truth. Everything else is derived.</em>
</p>

<p align="center">
  <a href="https://github.com/nerdsane/temper/actions/workflows/ci.yml"><img src="https://github.com/nerdsane/temper/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="LICENSE-MIT"><img src="https://img.shields.io/badge/license-MIT%2FApache--2.0-blue" alt="License"></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/rust-1.92%2B-orange" alt="Rust"></a>
  <a href="#whats-implemented"><img src="https://img.shields.io/badge/tests-590%2B-green" alt="Tests"></a>
</p>

---

**Temper** is a specification-first platform for building governed applications. You describe your domain — entities, states, transitions, guards, invariants — and Temper verifies the spec is correct, generates a full API, persists state through event sourcing, and evolves from production feedback.

The core idea: most backend applications are state machines. An order moves through Draft → Submitted → Shipped → Delivered. A support ticket goes from Open → InProgress → Resolved. If the state machine is the essential artifact, the surrounding infrastructure — persistence, API, authorization, observability, webhooks — follows mechanically from the specification.

Temper takes this seriously. **Specifications are the source of truth. Everything else is derived.**

## Two Ways to Use Temper

### As an Agent Operating Layer

Agents are getting good at acting — writing code, calling APIs, managing tasks. What they lack is an operating layer that governs what they do, records what they've done, and ensures they can't silently break things.

Temper provides that layer. Every state-changing action an agent takes flows through a governed, verified, auditable path:

- **Agents generate specifications** describing their plans as verified state machines — an agent cannot ship an order without payment captured, not because of a code review, but because the invariant was proven to hold across all reachable states before the spec was loaded
- **Cedar policies** control what each agent can do (default-deny posture; the human approves permission expansions as they arise)
- **Every action is recorded** with agent identity, before/after state, and the authorization decision that governed it
- **External access is sandboxed** through WASM integration modules, gated by Cedar policies

The agent is both developer and operator. The human is the policy setter.

### As an Application Platform

Build multi-tenant applications from specifications instead of code:

- Write or generate IOA specs for your entity types
- Run the four-level verification cascade to prove correctness before deployment
- Get a full **OData v4 API** with CRUD, filtering, pagination, and bound actions — automatically
- **Event-sourced persistence** to Postgres, Turso/libSQL, or Redis
- **Cedar authorization** on every action
- **OTEL observability** out of the box
- **Evolution engine** captures unmet user intents and proposes spec improvements

Same architecture, different deployment shape. What changes is who writes the specs and who sets the policies.

## How It Works

```
1. DESCRIBE         2. VERIFY            3. DEPLOY            4. EVOLVE

   IOA spec      →     L0: SMT        →    Actor runtime   →    Production
   (states,            L1: Model            (event sourced,      feedback
    actions,            check                governed,           → proposals
    guards,            L2: DST               OData API)         → developer
    invariants)        L3: PropTest                               approval
                                                                → hot-swap
```

### The Specification

Entities are defined in I/O Automaton TOML:

```toml
[automaton]
name = "Order"
states = ["Draft", "Submitted", "Shipped", "Delivered", "Cancelled"]
initial = "Draft"

[[state]]
name = "items"
type = "counter"
initial = "0"

[[action]]
name = "SubmitOrder"
from = ["Draft"]
to = "Submitted"
guard = "items > 0"

[[invariant]]
name = "SubmitRequiresItems"
when = ["Submitted", "Shipped", "Delivered"]
assert = "items > 0"

[[integration]]
name = "notify_fulfillment"
trigger = "SubmitOrder"
type = "webhook"
```

States, transitions, guards, invariants, and integrations — all in one declarative file. The framework never hardcodes entity-specific logic. All domain knowledge lives in the spec.

### The Verification Cascade

Every spec passes four levels before reaching production:

| Level | Method | What It Proves |
|-------|--------|----------------|
| **L0** | Z3 SMT | Guards satisfiable, invariants inductive, no unreachable states |
| **L1** | Stateright | Exhaustive state space exploration, safety + liveness properties |
| **L2** | Deterministic Simulation | Fault injection (delays, drops, crashes), reproducible via seeded PRNG |
| **L3** | Property-Based Testing | Random action sequences with shrinking to minimal counterexamples |

### The Runtime

Verified specs become transition tables that power an actor-based runtime:

- **Event sourcing** to Postgres, Turso, or Redis
- **OData v4 API** auto-generated from the spec's data model
- **Cedar authorization** evaluated on every state transition
- **OTEL telemetry** with wide events for every action
- **Webhook integrations** via outbox pattern
- **Hot-swap** — update specs live without dropping state

## Quick Start

```bash
# Clone and build
git clone https://github.com/nerdsane/temper.git
cd temper
cargo build --workspace

# Run the test suite (590+ tests)
cargo test --workspace

# Start with the reference e-commerce app (local file storage, no external deps)
TURSO_URL=file:local.db cargo run -- serve \
  --storage turso \
  --specs-dir reference-apps/ecommerce/specs \
  --tenant ecommerce

# Or with Postgres
DATABASE_URL=postgres://user:pass@localhost/db cargo run -- serve \
  --storage postgres \
  --specs-dir reference-apps/ecommerce/specs \
  --tenant ecommerce
```

### Verify a Spec

```bash
cargo run -- verify --specs-dir reference-apps/ecommerce/specs
```

### MCP Integration

```bash
cargo run -- mcp --app my-app=path/to/specs --port 3001
```

### Storage Backends

| Backend | Env Var | Best For |
|---------|---------|----------|
| **Postgres** | `DATABASE_URL` | Production, multi-tenant workloads |
| **Turso/libSQL** | `TURSO_URL` | Edge deployment, local dev (no cloud account needed) |
| **Redis** | `REDIS_URL` | Ephemeral / cache-oriented workflows |

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│                       temper-cli                        │
│                parse · verify · serve · mcp             │
├────────────────────────────┬────────────────────────────┤
│  temper-platform           │  temper-mcp                │
│  hosting, deploy pipeline, │  MCP server,               │
│  evolution integration     │  Monty Python sandbox      │
├────────────────────────────┼────────────────────────────┤
│  temper-server             │  temper-authz              │
│  HTTP/axum, OData,         │  Cedar policies,           │
│  entity dispatch           │  agent governance          │
├────────────────────────────┼────────────────────────────┤
│  temper-jit                │  temper-wasm               │
│  transition tables,        │  sandboxed integrations,   │
│  hot-swap controller       │  resource budgets          │
├────────────────────────────┼────────────────────────────┤
│  temper-runtime            │  temper-observe            │
│  actor system,             │  OTEL spans + metrics,     │
│  event sourcing            │  trajectory intelligence   │
├────────────────────────────┼────────────────────────────┤
│  temper-spec               │  temper-verify             │
│  IOA TOML + CSDL parser    │  L0-L3 cascade             │
├────────────────────────────┴────────────────────────────┤
│  temper-store-postgres · temper-store-turso · redis     │
│  temper-evolution · temper-optimize · temper-macros      │
└─────────────────────────────────────────────────────────┘
```

### Crates

| Crate | Purpose |
|-------|---------|
| **temper-spec** | IOA TOML + CSDL parsers, compiles to StateMachine IR |
| **temper-verify** | L0-L3 verification cascade (Z3, Stateright, DST, proptest) |
| **temper-jit** | TransitionTable builder, hot-swap controller, shadow testing |
| **temper-runtime** | Actor system, bounded mailboxes, event sourcing, SimScheduler |
| **temper-server** | HTTP/axum, OData routing, entity dispatch, webhooks, idempotency |
| **temper-odata** | OData v4: path parsing, query options, $filter/$select/$expand |
| **temper-authz** | Cedar-based authorization on every action |
| **temper-observe** | OTEL spans + metrics, ClickHouse adapter, trajectory tracking |
| **temper-evolution** | O→P→A→D→I record chain, Evolution Engine |
| **temper-wasm** | WASM sandboxed integrations with per-call resource budgets |
| **temper-mcp** | stdio MCP server with sandboxed code execution |
| **temper-platform** | Hosting platform, verify-deploy pipeline, system OData API |
| **temper-optimize** | Query + cache optimizer, N+1 detection, safety checker |
| **temper-store-postgres** | Postgres event journal + snapshots (multi-tenant) |
| **temper-store-turso** | Turso/libSQL event journal + snapshots |
| **temper-store-redis** | Redis mailbox streams, placement, distributed locks |
| **temper-cli** | CLI: parse, verify, serve, mcp, decide |
| **temper-codegen** | Code generation from CSDL (legacy path) |
| **temper-macros** | Proc macros: `#[derive(Message)]`, `#[derive(DomainEvent)]` |

## What's Implemented

| Feature | Status | Notes |
|---------|--------|-------|
| IOA TOML spec parser | **Done** | States, actions, guards, invariants, integrations |
| CSDL data model parser | **Done** | OData-compatible entity type definitions |
| Verification cascade (L0-L3) | **Done** | Z3 SMT, Stateright, DST with fault injection, proptest |
| Actor runtime + event sourcing | **Done** | Single-threaded, deterministic, bounded mailboxes |
| OData v4 API generation | **Done** | CRUD, $filter, $select, $expand, bound actions |
| Cedar authorization | **Done** | Default-deny, per-action policies, agent identity |
| OTEL observability | **Done** | Wide events, dual projection (metrics + spans) |
| Postgres / Turso / Redis backends | **Done** | Multi-tenant event journal + snapshots |
| MCP integration | **Done** | stdio server, sandboxed execution, agent governance |
| WASM sandboxed integrations | **Done** | Resource budgets, authorized host, Cedar-gated |
| Evolution Engine (O-P-A-D-I) | **Done** | Immutable record chain, developer approval gate |
| JIT transition tables + hot-swap | **Done** | Live spec updates without downtime |
| Agent governance UX | **Done** | Default-deny, human approval, pending decisions |
| Observe UI (Next.js) | **Done** | Decisions, agents, entities, specs, evolution pages |
| Webhook integrations | **Done** | Outbox pattern, retry, idempotency |

590+ tests across 19 crates. The reference e-commerce app (Order, Payment, Shipment) exercises the full stack end-to-end.

## What Temper Is Not

- **Not a general-purpose web framework.** Temper works for domains whose core logic is state-machine shaped. That covers a meaningful subset of enterprise applications, but not everything.
- **Not an agent framework.** Temper doesn't build agents. It's the operating layer agents run on top of — governing their actions, recording their state, verifying their plans.
- **Not a database.** Temper uses Postgres, Turso, or Redis for persistence. It adds a verified state machine layer on top.
- **Not a no-code tool.** Specifications are the interface, not drag-and-drop. They can be generated from conversation or written by hand.

## Known Limitations

| Limitation | Reason |
|------------|--------|
| State model is finite automaton (counters + booleans, no strings/floats) | By design — enables exhaustive verification |
| No cross-entity invariants | Integration engine orchestrates across entities |
| No temporal guards (time-based transitions) | Planned via integration engine |
| Single-node only | Redis placement traits designed, not yet wired |

## Roadmap

- [ ] **REPL interface** — Sandboxed code execution as the primary agent interface (Agentica/Code Mode style)
- [ ] **Multi-node deployment** — Distributed actor placement via Redis
- [ ] **Security review agents** — Delegated governance to oversight agents
- [ ] **Formal verification of WASM modules** — Extend the verification cascade to integrations
- [ ] **Cross-agent coordination** — Shared verified state as coordination primitive

## Reference Applications

| App | Entities | Demonstrates |
|-----|----------|--------------|
| [ecommerce](reference-apps/ecommerce/) | Order, Payment, Shipment | Full lifecycle: multi-state orders, payment capture, returns, fulfillment |
| [oncall](reference-apps/oncall/) | Page, EscalationPolicy, Postmortem, Remediation | Escalation workflows, incident management |

## Documentation

| Document | Description |
|----------|-------------|
| [Research Paper](docs/PAPER.md) | The full argument for spec-first development |
| [Positioning](docs/POSITIONING.md) | The observation that state machines are the essential artifact |
| [Agent Guide](docs/AGENT_GUIDE.md) | Technical reference for building with Temper |
| [Architecture Decisions](docs/adrs/) | 8 ADRs documenting major design choices |

## Contributing

Contributions are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
