# ADR-0035: IntentDiscovery Evolution Loop

## Status
Accepted

## Context
Temper already collects the raw ingredients for self-improvement: trajectories, denial decisions, system-wide evolution records, and spec-governed agents. What it does not have is a spec-governed orchestrator that turns those signals into repeatable product-intelligence work. The current sentinel and insight paths stop at threshold counting and ad hoc record creation.

The plan for this work is to close that loop with a Temper-native orchestrator that:
- is itself expressed as an IOA entity
- reads all relevant signals, not only failures
- delegates reasoning to `TemperAgent`
- persists the resulting O/P/A/I trail and PM issues
- can be triggered manually, by sentinel, and by future schedulers
- can be verified locally in mock mode and run for real with external model + observability credentials

## Decision
Introduce a new OS app entity, `IntentDiscovery`, as the system-owned evolution orchestrator.

`IntentDiscovery` is a state machine with the lifecycle:
`Triggered -> Gathering -> Analyzing -> Proposing -> Complete | Failed`

Its execution model is:
1. `Trigger` moves the entity into `Gathering` and runs `gather_signals`.
2. `gather_signals` reads the current signal surface from observe/OData endpoints and emits a compact signal summary.
3. `GatheringComplete` moves the entity into `Analyzing` and runs `spawn_analyst`.
4. `spawn_analyst` creates and provisions a `TemperAgent` configured with the evolution analyst prompt and the gathered signal summary, then waits for the agent to reach a terminal state through a bounded server-side wait endpoint.
5. `AnalysisComplete` moves the entity into `Proposing` and runs `create_proposals`.
6. `create_proposals` sends the structured agent output to a server-side materialization endpoint that persists O/P/A/I records and creates PM issues.
7. `ProposalComplete` finishes the cycle and records the created artifacts.

We also make four supporting changes:
- Sentinel now creates `IntentDiscovery` entities so anomaly detection feeds the intelligent loop instead of ending at observations.
- Policy suggestion patterns become tenant-scoped durable data in Turso rather than process-local memory.
- `TemperAgent` gains a deterministic `mock` provider so the full loop can still be proven locally without remote model credentials.
- Logfire is exposed to the analyst as a WASM-backed `logfire_query` tool instead of a Rust-only adapter, so observability drill-down stays inside the existing tool loop and uses Temper-managed secrets/config.

## Consequences
### Positive
- The evolution loop is now dogfooded through Temper’s own spec/runtime model.
- Evolution work becomes inspectable as first-class entity state, not opaque background code.
- Durable denial-pattern storage makes policy suggestions historical and tenant-scoped.
- End-to-end verification can run in CI and local worktrees because the analyst path has an offline mode, while real runs can use Anthropic plus Logfire-backed evidence.
- PM issues are created through the existing project-management OS app instead of a side channel.
- Logfire access is reusable as a generic agent tool instead of being hard-coded into the orchestrator.

### Negative
- The loop adds one more layer of orchestration and several new WASM modules to maintain.
- Sentinel-triggered analyses can create additional background work if not rate-limited by callers.
- The `mock` provider is intentionally heuristic and must never be confused with production-quality reasoning.
- The server now owns a generic wait endpoint for orchestration use, which expands the observe surface area and must stay bounded.

## Alternatives Considered
### Keep the logic in Rust handlers
Rejected. That would ship a second, non-spec-governed orchestration path and lose the dogfooding benefit.

### Call an external LLM directly from the evolution endpoint
Rejected. It would make verification brittle, credential-dependent, and harder to reproduce inside a worktree proof run.

### Add Logfire as a Rust-only adapter invoked outside the agent tool loop
Rejected. That would couple observability vendor semantics into the orchestration layer and bypass the existing `TemperAgent` tool architecture. A WASM-backed tool keeps auth/config in Temper and preserves a single reasoning/tooling model for agents.

### Persist only final suggestions, not raw denial patterns
Rejected. That loses tenant history, prevents recomputation when thresholds change, and keeps the suggestion endpoint semantically process-local.

## Implementation Notes
- `IntentDiscovery` is distributed as an OS app with its own IOA, CSDL, Cedar policy, and WASM modules.
- `POST /api/evolution/analyze` dispatches `Trigger` with `await_integration=true` so a single request can synchronously drive the full loop when the modules are installed.
- Record materialization stays server-side because it needs direct access to Temper’s record stores and entity dispatch internals.
- Real analyst runs use the existing `TemperAgent` loop with provider/model configured in `IntentDiscovery`; Logfire evidence is fetched through the WASM `logfire_query` tool.
- `spawn_analyst` relies on `GET /observe/entities/{entity_type}/{entity_id}/wait` for bounded waiting rather than hot polling from WASM.
