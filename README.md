<p align="center">
  <img src="assets/readme-hero.jpg" alt="Temper tempering plant" width="1000">
</p>

<p align="center">
  <em>A verified, policy-driven runtime for agents that build their own tools.</em>
</p>

<p align="center">
  <a href="https://github.com/nikstern/temper/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/nikstern/temper/ci.yml?style=for-the-badge&label=CI" alt="CI"></a>
  <a href="LICENSE-MIT"><img src="https://img.shields.io/badge/License-MIT%2FApache--2.0-blue?style=for-the-badge" alt="License"></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/Rust-1.92%2B-orange?style=for-the-badge" alt="Rust"></a>
  <a href="#status"><img src="https://img.shields.io/badge/Version-0.1.0-yellow?style=for-the-badge" alt="Pre-release"></a>
</p>

---

## Why Temper

Temper is a machine tool for agents.

Agents are fast at writing code, but the code drifts — invariants get missed, integrations break in ways no one anticipated. Temper inverts the path: agents describe what a system should do, and Temper builds the running version from the description.

The description hot-reloads. What an agent shipped keeps running while the agent revises it.

```text
Agent
  understands a need
    |
    | writes
    v
Spec
  state machine + WASM modules + policies + data
    |
    | verifies
    v
Kernel
  SMT + model checking + simulation + property tests
    |
    | deploys
    v
Runtime
  live state machine + typed API + audit log
    |
    | used by
    v
Same or another agent
  calls in, composes, writes the next spec

Tools build on tools.
```

---

## Quick start

The default path is a persistent, local-only daemon. It needs no account,
Docker, Postgres, Genesis server, or hosted deployment.

**Create and run an app:**

```bash
temper init my-app
cd my-app
temper dev
```

`temper dev` starts the local daemon when needed, verifies the workspace, and
promotes each valid revision as an immutable bundle. Verification failures leave
the last valid revision running. For separate daemon and install lifecycles:

```bash
temper up
temper app install ./my-app
```

The daemon persists under `~/.local/share/temper`, serves Observe at `/observe`,
and exposes authenticated stateless MCP at `/mcp`. See the
[local-first guide](docs/LOCAL_FIRST.md) for credentials, dependency locks,
cache maintenance, and optional Genesis/hosted progression.

**Connect a stdio-only agent client:**

```json
{
  "mcpServers": {
    "temper": {
      "command": "temper",
      "args": ["mcp", "--port", "3000"]
    }
  }
}
```

`temper mcp` is a stdio bridge that proxies to the running server and exposes a sandboxed Python REPL with a `temper.*` API for submitting specs, creating entities, and invoking actions.

**Or call directly:** the Rust SDK (`temper-sdk`), the TypeScript SDK (`packages/temper-sdk-ts`), or any HTTP client against `/tdata`.

When an agent invokes an action no policy permits, the request is denied and recorded as a pending decision. `temper decide` walks the queue and lets you approve at a chosen scope — narrow, medium, or broad. The new rule loads without restart.

---

## The shape of an app

A description in Temper has four parts.

- **Behavior** — what the system can do, and what it must never do. States, transitions, preconditions, and the safety properties that must hold in every reachable state.
- **Data contract** — what the system exposes to callers. Entity types, properties, relationships, and the actions each type supports — published as a typed API.
- **Authorization** — who can invoke which action on which resource under what conditions. Default-deny. Scope-based human approval, hot-loaded as the policy set grows.
- **Application logic** — what runs inside the state machine. Sandboxed modules with per-call resource budgets, triggered inline by transitions.

Each part is small enough for an agent to author and a human to read. The state machine is the contract; the application logic is what runs inside it.

```text
Description                          Runtime
+----------------------+             +----------------------+
| Behavior             |             | Typed API            |
| Data contract        |  verified   | Enforced state       |
| Authorization        | --------->  | machine              |
| Application logic    |  deployed   | Audit log            |
+----------------------+             +----------------------+
```

A heartbeat scheduler with one inline-triggered module — taken from `reference-apps/crucible`:

```toml
[automaton]
name = "CrucibleScheduler"
states = ["Idle", "Checking"]
initial = "Idle"

[[action]]
name = "Start"
kind = "input"
from = ["Idle"]
to = "Checking"
effect = [{ type = "trigger", name = "crucible_check_schedules" }]

[[action.triggers]]
name = "crucible_check_schedules"
kind = "wasm"
module = "crucible_scheduler_check"
on_success = "CheckComplete"
on_failure = "CheckFailed"
```

`Start` moves the entity from `Idle` to `Checking` and dispatches the WASM module. On success the runtime fires `CheckComplete`; on failure, `CheckFailed`. Either path is a verified transition.

---

## Hot reload

The kernel keeps running while specs and modules update. New revisions go live without a restart.

---

## Evolution

The system is split into a behavioral contract (the state machine) and the application logic (sandboxed modules). The split enables a self-improvement loop on either side, independent of the other.

---

## What gets verified

Every spec passes four layers before it is allowed to deploy.

- **Symbolic** — every guard satisfiable, every invariant inductive.
- **Model checking** — every reachable state visited; counterexamples printed on failure.
- **Simulation** — the production code path runs against a fault-injected sandbox; failures reproduce under deterministic seeds.
- **Property** — randomized action sequences with automatic counterexample shrinking.

Runs on every build, in well under a second on a small spec.

```bash
$ temper verify --specs-dir ./specs
L0 Symbolic:    PASSED  guards satisfiable, invariants inductive
L1 Model Check: PASSED  reachable states explored
L2 Simulation:  PASSED  fault-injected runs
L3 Property:    PASSED  randomized cases
```

---

## Built on Temper

- **[Katagami](https://github.com/arni-labs/katagami)** by [@arni0x9053](https://x.com/arni0x9053) — a library of agent-researched design languages. Each language ships as a verified spec plus a rendered embodiment of canonical UI elements. [Writeup](https://x.com/arni0x9053/status/2045594733654020449).
- **[Crucible](https://arunparthiban.substack.com/p/crucible-building-an-agentic-infrastructure)** by [Arun Parthiban](https://arunparthiban.substack.com/) — agentic infrastructure: agents, environments, sessions, governed lifecycle.

---

## Compatible agents

Temper exposes an HTTP API and a stdio MCP bridge. Anything that speaks HTTP or MCP can drive it.

Claude Code · OpenClaw · Pydantic AI · LangChain · custom HTTP / MCP clients

---

## Architecture

```
┌─────────────────────────────────────────────────────┐
│  Agent  (Claude Code, OpenClaw, custom, ...)        │
└────────────────────────┬────────────────────────────┘
                         │  MCP (optional)
                         ▼
┌─────────────────────────────────────────────────────┐
│  temper mcp  —  stdio bridge                        │
│  sandboxed Python REPL, temper.* API                │
└────────────────────────┬────────────────────────────┘
                         │  HTTP / OData
                         ▼
┌─────────────────────────────────────────────────────┐
│  Temper Kernel  (HTTP + OData server)               │
│                                                     │
│   Specs → Verify → Deploy                           │
│   AuthZ · WASM Triggers · Query                     │
│   Events · Observe · Evolve                         │
└─────────────────────────────────────────────────────┘
```

The kernel is static across deployments. Specs, data models, policies, and application modules are what change — and they hot-reload.

Full architecture in [docs/PAPER.md](docs/PAPER.md). Positioning in [docs/POSITIONING.md](docs/POSITIONING.md).

---

## What Temper is and is not

Temper is a verified runtime for the systems agents build.

|                                              |                                                                                                                                                            |
| -------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Not the runtime the agent runs in.**       | Hosted agent platforms and harness CLIs run the agent itself. Temper is what the agent reaches into from wherever it runs. *(You can also build a hosted agent runtime on Temper — Crucible does.)*                                                                                                                |
| **Not a framework for the agent loop.**      | SDKs and harnesses handle prompts, tools, and conversation. Temper holds what the loop calls into: verified state, governed integrations, typed APIs. *(An agent's state can also live entirely on Temper — see TemperPaw.)*                                                                                       |
| **Not a backend-as-a-service.**              | A BaaS gives you CRUD from a data schema; the rules of the system are implicit. Temper compiles a runtime from an explicit behavioral contract — legal transitions, required invariants — and verifies the contract before it runs.                                                                                |
| **Not a workflow builder.**                  | No imperative or visual flow editor. Capabilities are declared as verified state machines that any caller can use.                                                                                                                                                                                                  |

---

## Status

Version 0.1.0. The architecture is stabilizing; the API surface is not frozen. Deployed on Railway; Katagami runs on it in production.

---

## How it's implemented

<details>
<summary>The specification model, in detail</summary>

**Behavior** is expressed as an I/O Automaton specification (Nancy Lynch and Mark Tuttle, 1987), serialized as TOML and conventionally named `*.ioa.toml`. I/O Automata were chosen over TLA+ because the precondition/effect structure of actions maps directly onto how the runtime evaluates a transition, and the input/output/internal classification of actions maps cleanly onto how actors process messages. The same artifact is the verification target and the runtime execution artifact, which is the property that keeps proof and implementation aligned.

**Data contract** is expressed in CSDL (Common Schema Definition Language) from the OData v4 standard, serialized as XML and conventionally named `*.csdl.xml`. CSDL was chosen over GraphQL because agents need a rigid, machine-parseable contract rather than negotiated response shapes. A running Temper server publishes the full schema at `GET /tdata/$metadata`.

**Authorization** is expressed in Cedar, Amazon's declarative policy language. Cedar's `(principal, action, resource, context)` evaluation model maps onto the request structure exposed by the OData layer. Its default-deny posture is enforced at the policy engine, and generated policies from approved decisions are hot-loaded without downtime.

</details>

<details>
<summary>The verification cascade, in detail</summary>

- **L0 — symbolic reasoning.** Z3 SMT solver. Checks guard satisfiability (no dead transitions) and invariant inductiveness.
- **L1 — exhaustive model checking.** Stateright. Breadth-first exploration of the reachable state space; every reachable state is visited, every invariant is checked at every state.
- **L2 — deterministic simulation testing (DST).** The same Rust `TransitionTable` the server runs is executed against a simulated backend with seeded fault injection. Failures reproduce deterministically under the same seed.
- **L3 — property-based testing.** proptest. Randomized action sequences with automatic shrinking on failure.

</details>

<details>
<summary>Crate overview</summary>

| Crate | Purpose |
|-------|---------|
| **temper-spec** | IOA TOML + CSDL parsers, compiles to StateMachine IR |
| **temper-verify** | L0–L3 verification cascade (Z3, Stateright, DST, proptest) |
| **temper-jit** | TransitionTable builder, hot-swap controller |
| **temper-runtime** | Actor system, bounded mailboxes, event sourcing, SimScheduler |
| **temper-server** | HTTP/axum, OData routing, entity dispatch, idempotency |
| **temper-odata** | OData v4: path parsing, query options, `$filter`/`$select`/`$expand` |
| **temper-authz** | Cedar-based authorization engine |
| **temper-observe** | OTEL spans + metrics, trajectory tracking |
| **temper-evolution** | O-P-A-D-I record chain, evolution engine |
| **temper-wasm** | WASM sandboxed integrations with per-call resource budgets |
| **temper-mcp** | MCP server, Monty sandbox (execute tool) |
| **temper-platform** | Hosting platform, verify-deploy pipeline, skill catalog |
| **temper-optimize** | Query + cache optimizer, N+1 detection |
| **temper-store-postgres** | Postgres event journal + snapshots (multi-tenant) |
| **temper-store-turso** | Turso/libSQL event journal + snapshots |
| **temper-store-redis** | Distributed mailbox, placement, cache traits |
| **temper-cli** | CLI: parse, verify, serve, mcp, decide |
| **temper-sandbox** | Shared Monty sandbox infrastructure |
| **temper-sdk** | HTTP client library for Temper server |
| **temper-codegen** | Generates Rust actor code from CSDL + behavioral specs |
| **temper-store-sim** | In-memory deterministic event store with fault injection |
| **temper-wasm-sdk** | SDK for writing WASM integration modules |
| **temper-macros** | Proc macros: `#[derive(Message)]`, `#[derive(DomainEvent)]` |
| **temper-ots** | Open Trajectory Specification — DST-compatible trajectory capture for agent decisions |

</details>

---

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

Copyright (c) 2026 [Sesh Nalla](https://github.com/nerdsane) / [Rita Agafonova](https://github.com/rita-aga)
