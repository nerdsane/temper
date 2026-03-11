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
  <em>The framework where agents build their own OS.</em>
</p>

<p align="center">
  <a href="https://github.com/nerdsane/temper/actions/workflows/ci.yml"><img src="https://github.com/nerdsane/temper/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="LICENSE-MIT"><img src="https://img.shields.io/badge/license-MIT%2FApache--2.0-blue" alt="License"></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/rust-1.92%2B-orange" alt="Rust"></a>
  <a href="#whats-implemented"><img src="https://img.shields.io/badge/tests-950%2B-green" alt="Tests"></a>
</p>

---

Agents are starting to build their own tools — generating MCP servers at runtime, synthesizing helpers mid-session, evolving workflow topologies. At the same time, the infrastructure for making this safe is developing: policy-based authorization on tool invocations, behavioral contracts, state-machine-constrained agents, formal verification becoming practical. Harness engineering, durable execution, declarative agent specs — all moving forward.

Temper is our attempt to explore what happens when you connect these ideas into one framework: agent-created tools as formally verified state machines, authorization policies derived from behavioral specs, and an evolution loop where unmet intents feed back into spec proposals with human approval.

## How It Works

An agent describes what it needs as declarative specs — state machines, data models, integrations, authorization policies. Temper formally verifies the specs, deploys them as a live API, mediates every action through [Cedar](https://www.cedarpolicy.com/) policies, and records everything. The human approves or rejects. The agent operates through what it built.

```python
# Agent gives itself long-term memory — Temper verifies and deploys it
await temper.submit_specs("my-app", {
    "Knowledge.ioa.toml": knowledge_spec,   # state machine: agent-generated
    "model.csdl.xml": data_model            # data model: agent-generated
})
# → Verification cascade: Z3 SMT, model checking, simulation, property tests
# → If all levels pass, the knowledge system is live

# Agent stores and retrieves its own knowledge through the verified API
await temper.create("my-app", "KnowledgeEntries", {
    "content": "service X fails under concurrent writes — use advisory locks",
    "source": "incident-247"
})
await temper.action("my-app", "KnowledgeEntries", "k-42", "Link", {
    "related": ["k-12", "k-31"]   # connect insights across sessions
})
# → Cedar checks every operation — the agent can read its own entries
#   but can't access another agent's knowledge without approval
```

The kernel is a thin Rust runtime that interprets whatever the mediation pipeline feeds it. Everything agents touch — specs, policies, WASM modules, reaction rules — hot-reloads. The kernel itself rarely changes.

## Why Temper?

Models are an API call. The model-facing scaffolding — prompt templates, output parsers, tool wrappers — is being absorbed by smarter models. What remains is the world-facing infrastructure: state, authorization, verification, persistence. That's the layer that compounds.

Skills should be code with a signature. Harnesses should be too — and agents should be the ones writing and rewriting them.

| What's developing in the field | Temper's angle |
|---|---|
| Agents synthesize tools at runtime | Those tools are verified state machines that persist as specs |
| Policy-based authorization on tool invocations | Policies derived from a behavioral spec, not authored separately |
| Runtime guardrails check outputs | State machine checked exhaustively *before* deployment (model checking + SMT) |
| Observability shows what happened | Unmet intents feed back into spec proposals with human approval |
| Declarative agent specs for portability | Declarative specs for correctness — verified, then deployed |
| Durable execution engines | Spec defines what the system does; durability follows from event sourcing |
| Harnesses as static scaffolding | Harnesses as specs — agents program and rewrite them through the same verify-deploy loop |

It's an exploration of what happens when you put formal verification, Cedar authorization, and evolution feedback into the same loop.

## Key Features

### Spec-First Development

- Agents write declarative specifications, not application code
- IOA TOML specs define states, transitions, guards, and invariants; CSDL models define the data shape; Cedar policies define authorization
- The kernel derives all runtime behavior from these artifacts — if you lose the generated code, you regenerate it from the spec
- Specs hot-reload: transition tables, policies, WASM modules, and reaction rules update live

### Formal Verification

- Every spec passes a four-level cascade before it can deploy
- **L0 — Z3 SMT**: guards satisfiable, invariants inductive, no unreachable states
- **L1 — [Stateright](https://github.com/stateright/stateright) model checking**: exhaustive state space exploration, safety + liveness properties
- **L2 — Deterministic simulation**: fault injection (message delays, drops, crashes), reproducible via seeded PRNG
- **L3 — Property-based testing**: random action sequences with shrinking to minimal counterexamples
- The model checker verifies the same Rust code that runs in production — not a separate formal model

### Cedar Authorization

- Every action flows through [Cedar](https://www.cedarpolicy.com/) authorization with a default-deny posture
- Denied actions surface to the human as pending decisions — approve narrowly (this agent, this action, this resource), broadly (this agent, any action on this resource type), or deny
- Temper generates the Cedar policy from the approval; the human never writes policies from scratch
- Over time, the policy set converges on what the agent actually needs

### Self-Describing API

- Generated OData v4 endpoints with `$metadata` discovery
- Agents discover entity types, available actions, and valid transitions without documentation
- Full query support: `$filter`, `$select`, `$expand`, bound actions

### WASM Integrations

- External systems accessed through sandboxed WASM modules with per-call resource budgets
- Cedar mediates which integrations an agent can use — no raw API keys or direct network access
- Integrations declared in the spec, verified as part of the state machine

### Trajectories and Evolution

- Every action — success or failure — is recorded as a trajectory entry with agent identity, before/after state, and authorization decision
- The evolution engine analyzes trajectory patterns: repeated failures, friction points, unmet intents
- Patterns become spec proposals — an O-P-A-D-I record chain (Observation, Problem, Analysis, Decision, Impact) — surfaced for human approval
- The agent can propose changes to its own harness; the human holds the gate

## Quick Start

### For agents (via MCP)

Temper exposes a single MCP tool — `execute` — which runs Python in a sandboxed REPL against a running Temper server. The agent discovers specs, creates entities, invokes actions, and manages governance all through the `temper.*` API.

```python
# 1. Discover what's deployed
specs = await temper.specs("my-app")

# 2. Submit specs — the agent describes what it needs
await temper.submit_specs("my-app", {
    "Task.ioa.toml": task_spec,       # state machine
    "model.csdl.xml": data_model      # entity schema
})
# → Verification cascade runs automatically
# → If it passes, the API is live

# 3. Create entities and take actions
task = await temper.create("my-app", "Tasks", {
    "title": "Review PR #42",
    "assignee": "agent-codereview"
})
await temper.action("my-app", "Tasks", task["id"], "Start", {})

# 4. Query through OData
open_tasks = await temper.list(
    "my-app", "Tasks", "status eq 'InProgress'"
)
```

### For humans

Start a Temper server, then give your agent the MCP client. Add to your project's `.mcp.json`:

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

This gives the agent the `execute` tool — a sandboxed Python REPL with the `temper.*` API. The MCP server is a thin client that connects to a running Temper server.

```bash
temper serve --port 3000                        # start the server
temper mcp --port 3000                          # connect to local server
temper mcp --url https://temper.railway.app     # connect to remote server
temper mcp --port 3000 --agent-id bot           # set agent identity
```

Once agents are running, you manage them through the **Observe dashboard** (Next.js UI) or the CLI:

- **Decisions page**: When an agent hits a deny, you see the request and can approve at three scopes or deny. Temper generates the Cedar policy for you.
- **Agents page**: Action counts, denial rates, timelines.
- **Evolution page**: Spec proposals from the evolution engine. Approve to deploy, deny to discard.

```bash
temper serve --port 3000             # start the server
temper decide --list                 # see pending decisions
temper decide --approve <id> medium  # approve with medium scope
```

## Architecture

```
┌────────────────────────────────────────────────────────┐
│  Agent (Claude Code, Cursor, LangChain, CrewAI, etc.)  │
└───────────────────────┬────────────────────────────────┘
                        │  MCP (execute)
                        ▼
┌────────────────────────────────────────────────────────┐
│  Monty Sandbox (Python REPL)                           │
│  temper.submit_specs() · create() · action() · list()  │
└───────────────────────┬────────────────────────────────┘
                        │
                        ▼
┌────────────────────────────────────────────────────────┐
│  Temper Kernel                                         │
│                                                        │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐              │
│  │ Spec     │→ │ Verify   │→ │ Deploy   │              │
│  │ IOA+CSDL │  │ L0-L3    │  │ Actor RT │              │
│  └──────────┘  └──────────┘  └────┬─────┘              │
│                                   │                    │
│  ┌──────────┐  ┌──────────┐  ┌────▼─────┐              │
│  │ Cedar    │  │ WASM     │  │ OData    │              │
│  │ AuthZ    │  │ Integr.  │  │ API      │              │
│  └──────────┘  └──────────┘  └──────────┘              │
│                                                        │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐              │
│  │ Event    │  │ OTEL     │  │ Evolution│              │
│  │ Sourcing │  │ Telemetry│  │ Engine   │              │
│  └──────────┘  └──────────┘  └──────────┘              │
└────────────────────────────────────────────────────────┘
                        │
                        ▼
┌────────────────────────────────────────────────────────┐
│  Persistence: Postgres or Turso/libSQL                 │
└────────────────────────────────────────────────────────┘
```

**Hot-reloadable** (what agents create and modify):
- IOA specs → transition tables rebuild live
- Cedar policies → authorization engine reloads live
- WASM modules → re-instantiate live
- Reaction rules → reload live

**Static** (the kernel):
- Spec interpreter, Cedar evaluator, WASM host, HTTP server, persistence

## The Agent OS

The kernel is the foundation — spec interpreter, verification cascade, Cedar authorization, persistence. On top of it sit apps: sets of specs (state machines, data models, policies) verified and deployed on the kernel.

### Apps on the kernel

- **Bundled apps.** Some capabilities are general enough to ship with Temper: agent execution, task management, a notification pipeline. These arrive as pre-verified spec bundles — ready to use out of the box, or modify to fit.
- **Agent-built apps.** Others are specific to what the agent does. A deployment orchestrator for a DevOps agent. A patient intake workflow for a healthcare agent. The agent designs these as specs, submits them, and they become part of its operating environment.

### Composability

An agent's apps are entities on the same kernel. The task manager can reference knowledge entries. The code review workflow can spawn tasks. The notification pipeline can trigger on any state transition in any app. They compose because they share the same runtime, the same authorization model, and the same query surface (OData).

### Sharing

Everything is a spec, so agents can share them. An incident response workflow one agent built can be exported as a spec bundle and imported by another agent on another Temper instance. The verification cascade runs again on import, so the receiving agent knows the specs are sound in their context.

## How Agents Grow New Capabilities

Temper records every action — successes and failures — as trajectory entries. The evolution engine analyzes these trajectories for patterns and surfaces spec proposals for human approval. This creates a feedback loop where agents accumulate capabilities over time.

**Example: an agent keeps re-investigating solved bugs.** Trajectories show repeated context loss across sessions. The evolution engine surfaces the pattern. The agent designs a Knowledge spec (`Draft → Indexed → Linked → Archived`) with semantic search and Cedar-scoped access. You review the reachable states, approve, and the knowledge system hot-reloads. The agent starts retaining what it learns.

**Example: an agent hits a throughput bottleneck.** Trajectories show a growing queue of unprocessed work. The agent designs a TaskDelegation spec — entities that spawn scoped sub-agents with Cedar permissions narrowed to the delegated task. The spec's invariant guarantees a sub-agent can never escalate beyond its parent's authorization. You review, approve, and the agent can now distribute work.

The pattern repeats. Each cycle — trajectory analysis, spec proposal, verification, human approval — adds a new capability to the agent's operating environment.

## Running Agents

Temper already provides the shared state layer for multiple agents — verified entities queryable via OData, Cedar-mediated access between agents, and trajectories recording every action. The natural next step is building agent execution on top of these same primitives: modeling agents, tasks, and plans as Temper entities, with background execution, spawning, and coordination built in.

### Agents as entities

An Agent would be a Temper entity with its own state machine — just like any other entity. So would Plans, Tasks, and ToolCalls. Creating an agent, assigning it work, tracking its progress — all state transitions, all mediated by Cedar, all recorded as trajectories.

### Background execution

A headless executor daemon would watch for Agent entities via SSE, claim them, and run them concurrently:

- **Claiming.** Executor sets `executor_id` on the Agent entity — first-come-first-serve across multiple executor instances.
- **Concurrency.** Bounded by semaphore. Multiple executors share the load.
- **Fault tolerance.** Conversation state checkpointed after each turn. If an executor crashes, another resumes from the checkpoint.

### Spawning and coordination

- **Parent → child.** An agent spawns children through a `SpawnChild` action — same as creating any entity. The child gets a scoped role, goal, and Cedar permissions narrowed to its delegated task.
- **Cross-entity gates.** A parent's completion gates on all children reaching a terminal state — a cross-entity invariant verified before the spec deploys.
- **Shared state, not messaging.** Temper is the shared state layer. Agents coordinate by reading each other's entities through the same OData API. One agent's completed task unblocks another's next step — because they query the same verified state.

### Same primitives all the way down

The Agent state machine would be a spec. The Task lifecycle would be a spec. Cross-entity guards would be verified. Cedar would mediate every tool call. Trajectories would record every action. An agent spawning a child would go through the same verification-mediation-recording pipeline as an agent creating a knowledge entry.

### Where this is heading

Orchestration patterns as specs. What polls what, what supervises what, how agents form teams, what triggers a new agent — all expressible as state machines that go through the verification cascade. An agent could design its own orchestration topology, submit it, and have it verified before it runs.

## The Layers

Temper is being built bottom-up. Each layer enables the next.

| Layer | Description | Status |
|-------|-------------|--------|
| **6. Agent Execution** | Agents as entities. Background executor, spawning, scheduling, multi-agent coordination. | Planned |
| **5. Pure Temper Agent** | Agent's only tool is Temper. No raw shell, no bespoke tools. Everything mediated. | Planned |
| **4. Harness Composition** | Agents design harnesses as specs — what polls what, what reviews what, what gates what. | Planned |
| **3. Integration Framework** | Streaming-capable integrations (LLM calls, HTTP, databases) as WASM modules, mediated by Cedar. | In Progress |
| **2. Temper as Filesystem** | OData-queryable entity persistence replaces markdown files and JSON blobs. | Planned |
| **1. CRUD Apps** | Agents build applications as entity specs. Other agents consume them through the generated API. | Working |
| **Foundation: Kernel** | Spec parser, verification cascade, actor runtime, Cedar authZ, OData API, event sourcing, evolution. 950+ tests. | Done |

**Layer 1 — CRUD apps.** Temper entities are queryable via OData. An agent can build something like an issue tracker or project board entirely as Temper specs. Other agents consume it through the generated API. *Working today.*

**Layer 2 — Filesystem.** Agents tend to store state in markdown files, JSON blobs, or ad-hoc memory — fragile and unqueryable. If Temper's OData layer becomes the filesystem, every file is an entity, every write is a transition, every read is a query. Checkpointing becomes entity state. Version history becomes event sourcing. Search becomes `$filter`.

**Layer 3 — Integrations.** Agents need to reach external systems. Instead of bespoke tool implementations per agent, Temper provides an integration layer where agents write integrations as WASM modules + specs. Cedar mediates which integrations an agent can use.

**Layer 4 — Harness composition.** The harness should always be rewritable. With apps for tracking work, a filesystem for state, and integrations for external systems — agents have what they need to design complete harnesses as specs: what polls what, what reviews what, what gates what. Skills and harnesses are both code with a signature — declarative specs that agents author, verify, and rewrite as they evolve.

**Layer 5 — Pure Temper agent.** An agent whose only tool is Temper. No raw filesystem, no shell, no bespoke API clients. Everything mediated, queryable, auditable.

**Layer 6 — Agent execution.** The top of the stack: Temper runs the agents themselves. Agents as entities with verified state machines. Background executors claim and run them. Agents spawn children, schedule work, coordinate through shared state. The orchestration runs on the same primitives — specs, verification, Cedar, trajectories — as everything else.

## What's Implemented

| Feature | Status |
|---------|--------|
| I/O Automaton spec parser (states, actions, guards, invariants, integrations) | **Done** |
| CSDL data model parser (OData-compatible entity types) | **Done** |
| Verification cascade — L0 Z3 SMT, L1 Stateright, L2 DST with fault injection, L3 proptest | **Done** |
| Actor runtime with event sourcing, deterministic scheduling, bounded mailboxes | **Done** |
| OData v4 API generation (CRUD, $filter, $select, $expand, bound actions) | **Done** |
| Cedar authorization (default-deny, per-action policies, agent identity) | **Done** |
| OTEL observability (wide events, dual projection to metrics + spans) | **Done** |
| Postgres and Turso/libSQL persistence backends (multi-tenant) | **Done** |
| MCP integration — Monty sandbox with `execute` tool (thin client to running server) | **Done** |
| WASM sandboxed integrations (resource budgets, Cedar-gated) | **Done** |
| Evolution Engine — O-P-A-D-I record chain, unmet intent capture, approval gate | **Done** |
| JIT transition tables with hot-swap (live spec updates, zero downtime) | **Done** |
| Human approval flow (default-deny, pending decisions, Cedar policy generation) | **Done** |
| Observe dashboard — Next.js UI for decisions, agents, entities, specs, evolution | **Done** |
| Programmatic spec submission API (agents generate and deploy specs) | **Done** |
| Cross-entity choreography via reaction engine | **Done** |
| Agent runtime with LLM-driven execution loop and tool registries | In Progress |
| Headless executor — SSE-driven agent claiming, concurrent execution, checkpointing | Planned |
| Agent spawning — parent→child with cross-entity state gates and Cedar inheritance | Planned |
| Deterministic simulation store with configurable fault injection | **Done** |
| Temper as agent filesystem (OData-queryable entity persistence) | Planned |
| Streaming integration framework (LLM calls, HTTP, databases) | In Progress |
| Harness composition — agents design harnesses as specs | Planned |
| Formal verification of WASM integration modules | Planned |
| Cross-entity invariants (formal proofs spanning multiple entity types) | Planned |
| Orchestration patterns as specs — agent teams, supervision, triggers | Planned |
| Scheduled agent invocations — cron/timer-triggered execution | Planned |
| Distributed deployment — multi-node actor placement | Planned |

950+ tests across 25 crates.

<details>
<summary>What agents generate (IOA spec example)</summary>

Agents generate specs — nobody writes them by hand. But if you want to see what gets generated:

```toml
[automaton]
name = "Knowledge"
states = ["Draft", "Indexed", "Linked", "Archived"]
initial = "Draft"

[[state]]
name = "content"
type = "string"

[[state]]
name = "source"
type = "string"

[[state]]
name = "links"
type = "counter"
initial = "0"

[[action]]
name = "Index"
from = ["Draft"]
to = "Indexed"
guard = "content != ''"

[[action]]
name = "Link"
from = ["Indexed"]
to = "Linked"

[[action]]
name = "Archive"
from = ["Indexed", "Linked"]
to = "Archived"

[[invariant]]
name = "IndexRequiresContent"
when = ["Indexed", "Linked", "Archived"]
assert = "content != ''"

[[integration]]
name = "semantic_search"
trigger = "Index"
type = "wasm"
module = "search_service"
on_success = "IndexSucceeded"
on_failure = "IndexFailed"
```

States, transitions, guards, invariants, and WASM integrations — all in one declarative file. The verification cascade operates on this directly. The kernel derives a transition table from it.

</details>

<details>
<summary>Crate overview (25 crates)</summary>

| Crate | Purpose |
|-------|---------|
| **temper-spec** | IOA TOML + CSDL parsers, compiles to StateMachine IR |
| **temper-verify** | L0-L3 verification cascade (Z3, Stateright, DST, proptest) |
| **temper-jit** | TransitionTable builder, hot-swap controller, shadow testing |
| **temper-runtime** | Actor system, bounded mailboxes, event sourcing, SimScheduler |
| **temper-server** | HTTP/axum, OData routing, entity dispatch, webhooks, idempotency |
| **temper-odata** | OData v4: path parsing, query options, $filter/$select/$expand |
| **temper-authz** | Cedar-based authorization on every action |
| **temper-observe** | OTEL spans + metrics, trajectory tracking |
| **temper-evolution** | O-P-A-D-I record chain, Evolution Engine |
| **temper-wasm** | WASM sandboxed integrations with per-call resource budgets |
| **temper-mcp** | MCP server, Monty sandbox (execute tool, thin client) |
| **temper-platform** | Hosting platform, verify-deploy pipeline, system OData API |
| **temper-optimize** | Query + cache optimizer, N+1 detection, safety checker |
| **temper-store-postgres** | Postgres event journal + snapshots (multi-tenant) |
| **temper-store-turso** | Turso/libSQL event journal + snapshots |
| **temper-store-redis** | Distributed mailbox, placement, cache traits (stubs) |
| **temper-cli** | CLI: parse, verify, serve, mcp, decide |
| **temper-agent-runtime** | Agent execution loop with pluggable LLM providers and tool registries |
| **temper-executor** | Headless agent runner — watches for Agent entities, claims and executes them |
| **temper-sandbox** | Shared Monty sandbox infrastructure: JSON/Monty conversion, HTTP dispatch |
| **temper-sdk** | HTTP client library for Temper server (OData entities, governance API, SSE) |
| **temper-codegen** | Generates Rust actor code from CSDL entity models and behavioral specs |
| **temper-store-sim** | In-memory deterministic event store for simulation testing with fault injection |
| **temper-wasm-sdk** | SDK crate for writing WASM integration modules |
| **temper-macros** | Proc macros: `#[derive(Message)]`, `#[derive(DomainEvent)]` |

</details>

## Contributing

Contributions are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

Copyright (c) 2026 [Sesh Nalla](https://github.com/nerdsane) / [Rita Agafonova](https://github.com/rita-aga)
