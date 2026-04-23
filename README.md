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
  <em>A verified, policy-driven runtime for agents that build their own tools.</em>
</p>

<p align="center">
  <a href="https://github.com/nerdsane/temper/actions/workflows/ci.yml"><img src="https://github.com/nerdsane/temper/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="LICENSE-MIT"><img src="https://img.shields.io/badge/license-MIT%2FApache--2.0-blue" alt="License"></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/rust-1.92%2B-orange" alt="Rust"></a>
  <a href="#status"><img src="https://img.shields.io/badge/version-0.1.0-yellow" alt="Pre-release"></a>
</p>

---

## Why this exists

Agents can produce code faster than any team can review by hand. That would be acceptable if what they produced were reliably correct, but it is not, and the gap between what an agent generates and what would pass verification is where the failure modes accumulate — hallucinated dependencies in the obvious case, and in the less obvious cases, missing invariants and unseen race conditions that the agent cannot encounter because it never actually runs what it writes. Wrapping an agent around a traditional codebase converts this into a throughput problem without closing the verification gap itself.

Temper inverts the relationship. Agents do not produce application code; they produce specifications. The kernel reads each specification, verifies it through four layers of analysis, and deploys the running system the specification describes. Because the specification is both the artifact that gets proved and the artifact that gets executed, there is no drift between what was verified and what is running.

## The specification model

A capability in Temper is described by three artifacts, each addressing a different concern.

**Behavior — what the system can do, and what it must never do.** The entity's states, the transitions between them, the preconditions on each transition, and the safety properties that must hold in every reachable state. This is declared as a state machine specification (see [*How it's implemented*](#how-its-implemented) for the formalism and file format).

**Data contract — what the system exposes to callers.** The entity types, their properties and relationships, and the actions each type supports. A running Temper server publishes this contract in a machine-parseable form so that an agent can discover the full API surface without documentation or examples.

**Authorization — who can invoke which action on which resource under what conditions.** The model is default-deny with scope-based approval. When an agent attempts an action the current policy set does not permit, the denial is recorded as a pending decision. A human approves at a chosen scope — narrow to this case, medium to this action and resource type, broad to the resource type — and the resulting rule is hot-loaded into the policy engine.

The three artifacts together are what an agent submits. The kernel verifies that combination before anything runs.

## The verification cascade

Every spec passes four independent layers of analysis before the kernel will load it.

1. **Symbolic reasoning** — proves that each guard is satisfiable (no dead transitions) and that each safety invariant is inductive (if it holds before any guard-satisfying transition, it holds after).
2. **Exhaustive state exploration** — visits every reachable state of the specification and checks every invariant at every state. If a path exists from the initial state to a violation, the counterexample is printed.
3. **Deterministic simulation** — runs the actual production code path against a simulated backend with seeded fault injection: message drops, delays, reordering, crashes. Failures reproduce exactly under the same seed, which makes them debuggable.
4. **Randomized property testing** — a thousand pseudorandom action sequences of up to thirty steps, with invariants checked after every step. Any violation is automatically shrunk to a minimal counterexample.

On a small spec the whole cascade runs in well under a second. It runs on every build, not as a gate applied before shipping.

## A Label, end to end

The smallest useful entity has two states, one transition, and one safety property. The behavioral spec:

```toml
# label.ioa.toml
[automaton]
name = "Label"
states = ["Active", "Archived"]
initial = "Active"

[[action]]
name = "Archive"
kind = "input"
from = ["Active"]
to = "Archived"

[[invariant]]
name = "ArchivedIsFinal"
when = ["Archived"]
assert = "no_further_transitions"
```

With a matching data contract and policy, verification runs:

```bash
$ temper verify --specs-dir ./specs
L0 Symbolic:          PASSED  2 guards satisfiable, 1 invariant inductive
L1 Model Check:       PASSED  All reachable states explored
L2 Simulation:        PASSED  10 seeds, 47 transitions, 0 violations
L3 Property Tests:    PASSED  1,000 cases, 30 max steps per case

$ temper serve --specs-dir ./specs
```

The server now exposes the entity over HTTP. Calling the archive action a second time on an already-archived label returns a 409 Conflict in roughly 28 nanoseconds, because the runtime's transition table has no rule with `from = ["Archived"]`. The invariant that the symbolic layer proved inductively and the model checker explored exhaustively is the same artifact the runtime enforces at the guard check. There is no controller-level guard to write, and therefore no place for the implementation to drift from the specification.

The workflow does not change as entities grow. A ten-state Issue with cross-entity relationships, role-separated policies for planners and reviewers, and sandboxed integrations for side effects uses the same three artifacts and the same cascade.

## A real application: Katagami

[@arni0x9053](https://x.com/arni0x9053) built [Katagami](https://x.com/arni0x9053/status/2045594733654020449) on Temper. It is a library of agent-researched design languages — each one a full specification of philosophy, tokens, compositional rules, layout principles, and usage guidance, paired with a rendered embodiment of roughly fifteen canonical UI elements in that style. The motivation is to eliminate the cold-start problem when asking an agent to style a project, by giving both parties a shared vocabulary drawn from named movements rather than improvised prose.

A single research prompt fans out into parallel synthesize sessions. Each session writes its specification and generates a rendered embodiment inside a cloud sandbox, visually verified at three viewport sizes. The agent's calls into Temper are state machine transitions:

```python
lang = temper.create('DesignLanguages', {'Id': 'retro-futurism-crt'})
eid = lang['entity_id']

temper.action('DesignLanguages', eid, 'WritePhilosophy', {
    'philosophy': json.dumps(philosophy)
})
# SetTokens, SetRules, SetLayout, SetGuidance, ...

temper.action('DesignLanguages', eid, 'SubmitForReview', {})
temper.action('DesignLanguages', eid, 'Publish', {})
```

The `SubmitForReview` transition has a guard that requires all five spec sections and the rendered embodiment to exist. An incomplete language cannot be submitted because the precondition is part of the verified specification, not a check an agent could forget to write into application code.

Katagami is deployed in production on Railway, with observability piped out to Datadog and storage split between a transactional store and a blob store. When a batch of jobs started timing out with only "dispatch timeout" in the logs, the root cause turned out to be contention in the shared actor registry under concurrent load — retries exhausted their attempt budget before requests got through. The event stream, the trace, and the state of every entity were all queryable, which is the property that made the diagnosis possible from the agent's side rather than from a human reading dashboards.

[Arun Parthiban](https://arunparthiban.substack.com/p/crucible-building-an-agentic-infrastructure) is building **Crucible** on the same foundation, using Temper as the control plane for agent sessions, environments, and delegation graphs — where Katagami uses Temper to govern what agents produce, Crucible uses it to govern the agents themselves. The writeup goes into the two-layer architecture in detail.

## Quick start

Temper runs as an MCP server. The agent gets a sandboxed Python REPL with a `temper.*` API that submits specs, creates entities, invokes actions, and inspects pending governance decisions.

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
temper serve --port 3000          # start the kernel and Observe UI
temper decide --port 3000         # interactive review of pending decisions
```

When an attempted action has no permitting policy, the request is denied and recorded as a pending decision. `temper decide` opens an interactive prompt that walks through each pending decision and lets you approve at a scope — narrow to this case, medium to this action and resource type, broad to the resource type — or deny. The resulting rule is generated and hot-loaded, and subsequent attempts in that scope succeed. The policy set converges on what the agent actually needs over time rather than requiring you to anticipate every permission in advance.

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

The kernel is static across deployments. It contains the spec interpreter, the verification cascade, the actor runtime with bounded mailboxes, the authorization engine, the event-sourced store, and the observability plane. Skills — the state machines, data models, policies, and integrations that make up an application — are what agents create and modify, and they hot-reload without the kernel coming down.

## What Temper is and is not

Temper is a verified state machine runtime that exposes a backend. Katagami uses it as a backend, so the honest characterization is that it is one — but the contract is the specification, and the runtime enforces a state machine rather than dispatching handlers attached to CRUD operations. A few distinctions worth making explicit:

|                                              |                                                                                                                                                            |
| -------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Not an agent framework.**                  | Temper does not build agents. You bring your own — Claude Code, OpenClaw, Pydantic AI, LangChain, or any client that speaks MCP.                           |
| **Not a schema-first backend-as-a-service.** | Conventional BaaS generates CRUD from a schema. Temper generates a verified runtime from a specification that includes behavior, guards, and invariants. |
| **Not a workflow builder.**                  | There is no imperative or visual flow editor. Capabilities are declared as verified state machines.                                                         |
| **Not a prompt manager.**                    | Prompts, models, and agent runtimes stay with you. Temper governs what agents do to the world, not what they say.                                          |

## Status

Temper is at version 0.1.0. The architecture is stabilizing, the API surface is not frozen, and there are 1,300+ tests across 25 crates. The agent execution layer is deployed on Railway and Katagami runs on it in production.

| Working | Next |
|---|---|
| Spec parser and four-layer verification cascade | Agents as entities with a background executor |
| Authorization engine with default-deny and scoped approval flows | Streaming integrations |
| API generation with schema discovery | Harness composition (agents designing harnesses as specs) |
| Event sourcing on Postgres and Turso/libSQL | Distributed deployment |
| MCP integration via sandboxed REPL | |
| Sandboxed integrations with per-call resource budgets | |
| Evolution engine — trajectory capture, failure clustering, spec proposals | |
| Observe dashboard | |
| Pre-built skills: project management, filesystem, agent orchestration | |

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
<summary>Crate overview (25 crates)</summary>

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
| **temper-transport** | Platform-agnostic channel transports (e.g., Discord) that bridge external messaging to Temper's Channel entities |

</details>

---

## Contributing

Contributions are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

Copyright (c) 2026 [Sesh Nalla](https://github.com/nerdsane) / [Rita Agafonova](https://github.com/rita-aga)
