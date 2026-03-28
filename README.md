<h1 align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/mascot-vectorized-light.svg">
    <source media="(prefers-color-scheme: light)" srcset="assets/mascot-vectorized.svg">
    <img src="assets/mascot-vectorized.svg" width="140" alt="Temper">
  </picture>
  <br>
  Temper
</h1>

<p align="center">
  <em>A verified operating layer for autonomous agents</em>
</p>

<p align="center">
  <a href="https://github.com/nerdsane/temper/actions/workflows/ci.yml"><img src="https://github.com/nerdsane/temper/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="LICENSE-MIT"><img src="https://img.shields.io/badge/license-MIT%2FApache--2.0-blue" alt="License"></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/rust-1.92%2B-orange" alt="Rust"></a>
  <a href="#status"><img src="https://img.shields.io/badge/version-0.1.0-yellow" alt="Pre-release"></a>
</p>

---

## What is Temper?

Agents build tools at runtime. They generate helpers and create workflows. Those tools have no verification, no governance, no memory of why they exist.

Temper is an operating layer where agents describe capabilities as specifications. The kernel verifies each spec before deployment. Every action flows through authorization policies. A human approves changes to scope.

As agents and users operate through a skill, the evolution engine identifies gaps. It adds missing capabilities, fixes broken ones, and removes redundant ones. The human approves each change.

|        | Step            | What happens                                                            |
| ------ | --------------- | ----------------------------------------------------------------------- |
| **01** | Describe        | An agent describes what it needs: states, transitions, guards, data shape. |
| **02** | Verify          | The kernel proves the spec is sound before anything runs.                |
| **03** | Operate         | The agent works through the verified API. Every action is governed and recorded. |
| **04** | Evolve          | Usage patterns surface gaps. The spec adapts. The human approves.        |

<br/>

## Constructor, description, evolution

In 1949, Von Neumann designed a self-replicating machine with three parts: a *description* (the blueprint), a *constructor* that reads any description and builds the machine it encodes, and a copy mechanism that duplicates and mutates descriptions over time. The machine grows in complexity by changing its descriptions. The constructor stays the same.

Temper follows this pattern.

The **kernel** is the constructor. It reads specifications, verifies them, and deploys actors. It does not know what you are building. It interprets whatever you feed it.

**Skills** are the descriptions. Each skill bundles a verified state machine, a data model, authorization policies, and integration declarations into a single deployable capability. Agents create new skills by describing what they need. Other agents and users operate through them.

The **evolution engine** observes how agents use skills, clusters failure patterns, and proposes spec changes. Agents can also create new skills when they encounter problems the current set does not cover.

<br/>

## Temper is right for you if

- ✅ You give agents tools and worry about what those tools do unsupervised
- ✅ You want agents to create their own capabilities, with proof those capabilities are safe
- ✅ You need an audit trail connecting every agent action to an authorization decision
- ✅ You want agent-built tools to improve through use, without manual rewrites
- ✅ You're building multi-agent systems that need shared, governed state
- ✅ You want a default-deny security posture where permissions grow as trust builds

<br/>

## Features

<table>
<tr>
<td align="center" width="33%">
<h3>Verified Skills</h3>
Agents describe capabilities as specs. A four-level verification cascade proves them sound before deployment.
</td>
<td align="center" width="33%">
<h3>Governed by Default</h3>
Every action flows through authorization with a default-deny posture. Denied actions surface to the human for approval. The policy set grows as the agent works.
</td>
<td align="center" width="33%">
<h3>Self-Evolving</h3>
The evolution engine observes usage patterns and failures. It proposes spec changes. Agents create new skills. The human approves every change.
</td>
</tr>
<tr>
<td align="center">
<h3>Self-Describing API</h3>
Every skill generates a queryable API with schema discovery. Agents find available actions and valid transitions without documentation.
</td>
<td align="center">
<h3>Full Audit Trail</h3>
Every action records agent identity, before/after state, and the authorization decision. Agents can query their own history.
</td>
<td align="center">
<h3>Hot-Reload</h3>
Skills deploy and update without downtime. Specs, policies, and integrations reload live.
</td>
</tr>
</table>

<br/>

## Without Temper vs. With Temper

| Without Temper | With Temper |
|---|---|
| ❌ Agents build tools with no proof those tools are correct | ✅ Every tool is a verified state machine, proven sound before it runs |
| ❌ Agent permissions live in prompts and hope | ✅ Authorization policies enforce boundaries. Denied actions surface for human approval |
| ❌ Agent state lives in markdown files and JSON blobs | ✅ State lives in event-sourced entities with queryable APIs |
| ❌ No audit trail for agent actions | ✅ Every action records who did what, when, under which policy |
| ❌ Adding agent capabilities means writing code | ✅ Agents describe new capabilities. The kernel verifies and deploys them |
| ❌ Tools break silently | ✅ The verification cascade catches violations before deployment |

<br/>

## What Temper is not

|                              |                                                                                                                      |
| ---------------------------- | -------------------------------------------------------------------------------------------------------------------- |
| **Not an agent framework.**  | Temper does not build agents. It provides the layer agents run on. Bring your own: Claude Code, OpenClaw, Pydantic AI, LangChain, or anything with MCP support. |
| **Not a workflow builder.**  | No drag-and-drop pipelines. Temper models capabilities as verified state machines. |
| **Not a backend-as-a-service.** | Temper generates APIs from specifications. You do not write controllers or service layers. |
| **Not a prompt manager.**    | Agent prompts, models, and runtimes are yours. Temper governs what agents *do*. |

<br/>

## Quick start

Add Temper as an MCP server. Your agent gets a sandboxed Python REPL with the `temper.*` API.

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

```bash
temper serve --port 3000          # start the kernel
```

Through the REPL, agents discover deployed skills, create entities, invoke actions, submit new specifications, and manage governance. You manage pending decisions through the Observe dashboard or CLI.

```bash
temper decide --list              # see pending authorization decisions
temper decide --approve <id>      # approve with a scope
```

<br/>

## Architecture

```
┌─────────────────────────────────────────────────────┐
│  Agent (Claude Code, OpenClaw, Pydantic AI, etc.)   │
└────────────────────────┬────────────────────────────┘
                         │  MCP (execute)
                         ▼
┌─────────────────────────────────────────────────────┐
│  Sandboxed REPL                                     │
│  temper.submit_specs() · create() · action() · ...  │
└────────────────────────┬────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────┐
│  Temper Kernel                                      │
│                                                     │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐          │
│  │ Specs    │→ │ Verify   │→ │ Deploy   │          │
│  └──────────┘  └──────────┘  └────┬─────┘          │
│                                   │                 │
│  ┌──────────┐  ┌──────────┐  ┌────▼─────┐          │
│  │ AuthZ    │  │ Integr.  │  │ Query    │          │
│  └──────────┘  └──────────┘  └──────────┘          │
│                                                     │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐          │
│  │ Events   │  │ Observe  │  │ Evolve   │          │
│  └──────────┘  └──────────┘  └──────────┘          │
└─────────────────────────────────────────────────────┘
```

**The kernel** (static): spec interpreter, verification cascade, actor runtime, authorization engine, event sourcing, telemetry.

**Skills** (what agents create and modify): state machines, data models, authorization policies, integrations. All hot-reloadable.

<br/>

## Status

> **Temper is pre-release (0.1.0).** The architecture is stabilizing. The API surface is not frozen. Expect breaking changes. We are building and exploring.

| Working | Next |
|---|---|
| Spec parser and verification cascade (SMT, model checking, simulation, property tests) | Agent execution (agents as entities, background executor) |
| Authorization engine (default-deny, approval flows, policy generation) | Streaming integrations |
| API generation with schema discovery | Harness composition (agents design harnesses as specs) |
| Event sourcing (Postgres, Turso/libSQL) | Distributed deployment |
| MCP integration (sandboxed REPL) | |
| Sandboxed integrations with resource budgets | |
| Evolution engine (trajectory capture, failure clustering, spec proposals) | |
| Observe dashboard (decisions, agents, entities) | |
| Pre-built skills: project management, filesystem, agent orchestration | |

950+ tests across 25 crates.

<details>
<summary>Technical details for the curious</summary>

### Verification cascade

Every spec passes four levels before deployment:

- **L0**: SMT solver checks guard satisfiability and invariant inductiveness
- **L1**: Exhaustive model checking explores the full state space
- **L2**: Deterministic simulation with fault injection (message drops, delays, crashes)
- **L3**: Property-based testing with random action sequences and shrinking

The model checker verifies the same Rust code that runs in production.

### Specifications

Skills are defined by three declarative artifacts:

- **I/O Automaton specs** (.ioa.toml): states, transitions, guards, invariants, integration declarations
- **CSDL data models** (.csdl.xml): entity types, relationships, actions (OData v4 standard)
- **Cedar policies** (.cedar): authorization rules with default-deny posture

Agents generate these. Nobody writes them by hand.

### Crate overview (25 crates)

| Crate | Purpose |
|-------|---------|
| **temper-spec** | IOA TOML + CSDL parsers, compiles to StateMachine IR |
| **temper-verify** | L0-L3 verification cascade (Z3, Stateright, DST, proptest) |
| **temper-jit** | TransitionTable builder, hot-swap controller |
| **temper-runtime** | Actor system, bounded mailboxes, event sourcing, SimScheduler |
| **temper-server** | HTTP/axum, OData routing, entity dispatch, idempotency |
| **temper-odata** | OData v4: path parsing, query options, $filter/$select/$expand |
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
| **temper-agent-runtime** | Agent execution loop with pluggable LLM providers |
| **temper-executor** | Headless agent runner (watches for Agent entities, claims and executes) |
| **temper-sandbox** | Shared Monty sandbox infrastructure |
| **temper-sdk** | HTTP client library for Temper server |
| **temper-codegen** | Generates Rust actor code from CSDL + behavioral specs |
| **temper-store-sim** | In-memory deterministic event store with fault injection |
| **temper-wasm-sdk** | SDK for writing WASM integration modules |
| **temper-macros** | Proc macros: `#[derive(Message)]`, `#[derive(DomainEvent)]` |

</details>

<br/>

## Contributing

Contributions are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

Copyright (c) 2026 [Sesh Nalla](https://github.com/nerdsane) / [Rita Agafonova](https://github.com/rita-aga)
