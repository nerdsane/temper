# ADR-0031: Agent Orchestration OS App and Native Adapter Integrations

- Status: Proposed
- Date: 2026-03-14
- Deciders: Temper core maintainers
- Related:
  - ADR-0027: OS app catalog
  - ADR-0017: deterministic simulation testing
  - `crates/temper-platform/src/os_apps.rs`
  - `crates/temper-server/src/state/dispatch/*`

## Context

Temper already supports governed entities, policy enforcement, and verified workflows, but it does not yet provide a first-class OS app for multi-agent orchestration with explicit budget and organization controls. External orchestration systems implement these capabilities with bespoke service logic and adapter layers.

That approach duplicates capabilities Temper already has (state machines, policies, verification, tenant isolation), and creates operational drift between platform workflows and orchestration workflows.

A second gap is integration capability: `type = "wasm"` is intentionally sandboxed and cannot run local CLIs or maintain full-duplex gateway sessions needed by local agent executors and remote gateway adapters.

## Decision

Temper will ship a new OS app, `agent-orchestration`, and add a native integration type, `type = "adapter"`, executed by platform-level Rust adapters.

### Sub-Decision 1: Add Agent Orchestration as an OS App

Add new entity specs and model definitions for:
- `HeartbeatRun` for heartbeat/execution lifecycle
- `Organization` for membership and budget controls
- `BudgetLedger` for append-only spend audit records

Add Cedar policies that separate scheduling/budget approval authority from execution authority.

**Why this approach**: This keeps orchestration governed by the same spec + policy + verification system as every other Temper domain.

### Sub-Decision 2: Add `adapter` Integration Type in Server Dispatch

Extend post-dispatch integration execution to support:
- Existing `type = "wasm"`
- New `type = "adapter"`

`adapter` integrations resolve an adapter implementation from a registry and execute it with resolved integration config, entity state, and agent context.

**Why this approach**: Native adapters can execute capabilities unavailable to WASM (process spawn, richer network transports) while preserving IOA-declared integration intent.

### Sub-Decision 3: Introduce Built-In Adapter Implementations

Provide platform adapter modules for:
- Claude Code CLI
- Codex CLI
- OpenClaw gateway WebSocket
- Generic HTTP webhook execution

Adapters share a common trait and result shape, enabling registry-driven dispatch.

**Why this approach**: A shared interface enables testing, fallback, and future adapters without changing dispatch semantics.

## Rollout Plan

1. **Phase 0 (Immediate)**
   - Add ADR.
   - Add agent + agent type spec extensions and CSDL fields.
   - Add `agent-orchestration` OS app specs, Cedar policy, model, and catalog registration.
2. **Phase 1 (Integration Dispatch)**
   - Add adapter trait/registry and dispatch wiring.
   - Add built-in adapters.
3. **Phase 2 (Validation and Hardening)**
   - Add/expand tests for OS app registration and adapter dispatch routing.
   - Run platform/server test suites.

## Readiness Gates

- New OS app specs parse and pass verification cascade.
- OS app install registers all agent-orchestration entity types.
- `type = "adapter"` dispatch executes and routes callbacks.
- Existing WASM integration behavior is unchanged.

## Consequences

### Positive

- Multi-agent orchestration becomes a native governed app.
- Adapter execution gains feature parity paths not possible in WASM.
- Budget and org controls become explicit state-machine behavior.

### Negative

- Server complexity increases with a second integration execution path.
- Native adapters require tighter operational security controls than WASM sandboxing.

### Risks

- Adapter execution failures or misconfiguration can create callback churn.
- Additional dependencies increase maintenance surface.

Mitigations:
- Registry-based adapter resolution with explicit errors.
- Guarded callback dispatch and telemetry logs.
- Cedar policy separation for scheduling/approval/execution actions.

### DST Compliance

Simulation-visible crate changes occur in `temper-server` dispatch/adapters.
- Deterministic data structures use `BTreeMap` in registries/config maps.
- Process and WebSocket side effects are explicitly annotated with `// determinism-ok` because they are external side effects outside simulation core state transitions.
- Core state machine execution remains deterministic.

## Non-Goals

- Replacing existing WASM integration support.
- Building tenant-specific adapter plugins in this ADR.
- Defining a complete pricing/accounting model beyond counter-based budget tracking.

## Alternatives Considered

1. **WASM-only orchestration adapters**
   - Rejected because local CLI process control and richer gateway workflows are not practical in current WASM host constraints.
2. **Separate orchestration microservice outside Temper**
   - Rejected because it duplicates governance and state semantics already present in Temper.

## Rollback Policy

If adapter dispatch proves unstable, disable `type = "adapter"` execution path in dispatch while retaining OS app specs. Existing entity behavior remains available, and integrations can be migrated to `wasm` or webhook workflows where applicable.
