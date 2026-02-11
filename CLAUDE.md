# Temper — Claude Code Project Guide

## What is Temper?
A conversational application platform. Developers describe what they want through conversation — the system generates specs, verifies them, and deploys entity actors. End users interact through a separate production chat. Unmet user intents feed back through the Evolution Engine for developer approval.

## The Vision
```
Developer Chat: "I want a project management tool"
  → System interviews developer about entities, states, actions, guards
  → Generates IOA specs + CSDL + Cedar from conversation
  → Runs 3-level verification cascade
  → Hot-deploys entity actors + OData API

Production Chat: end users operate the app
  → Unmet intents → trajectory spans → ClickHouse → Sentinel
  → O-Record → I-Record → Developer reviews → D-Record → spec change
```

Two separated contexts: Developer Chat (design-time, can modify specs) and Production Chat (runtime, operates within specs). The developer holds the approval gate for all behavioral changes.

## Architecture
- **temper-spec**: I/O Automaton TOML parser + CSDL parser
- **temper-verify**: Stateright model checking, deterministic simulation, property tests
- **temper-jit**: TransitionTable builder from IOA specs (no verification deps in production)
- **temper-runtime**: Actor system, SimScheduler, SimActorSystem, sim_now()/sim_uuid(), TenantId
- **temper-server**: HTTP server, EntityActor, EntityActorHandler, SpecRegistry (multi-tenant)
- **temper-observe**: WideEvent telemetry (OTEL spans + metrics), trajectory tracking
- **temper-evolution**: O-P-A-D-I record chain, Evolution Engine
- **temper-store-postgres**: Event sourcing persistence (tenant-scoped)
- **temper-store-redis**: Mailbox and placement cache (tenant-scoped)

## Key Rules

### Platform Philosophy
- Specs are generated from conversation, never hand-written by developers
- Code is derived from specs and is regenerable
- Framework code must NOT hardcode entity-specific state names
- Domain invariants come from the spec's [[invariant]] sections
- Trajectory intelligence captures every unmet intent
- The verification cascade gates every spec change

### Spec Format
- I/O Automaton TOML (`.ioa.toml`) is the primary spec format
- Use `TransitionTable::from_ioa_source()` in production
- TLA+ is legacy — `from_tla_source()` is `#[cfg(test)]` only

### Multi-Tenancy
- SpecRegistry maps (TenantId, EntityType) → specs + TransitionTable
- Postgres/Redis are tenant-scoped
- Single-tenant uses TenantId::default() = "default"

### Deterministic Simulation
- Use `sim_now()` / `sim_uuid()` instead of wall clock / random UUIDs
- Use `BTreeMap` not `HashMap` in simulation-visible code
- `SimActorHandler::spec_invariants()` auto-checks [[invariant]] sections

### Dependency Discipline
- `temper-jit` must NOT depend on `temper-verify` in `[dependencies]`
- Production binaries must not pull in `stateright` or `proptest`

### Rust Conventions
- Edition 2024, rust-version 1.85
- `gen` is a reserved keyword — never use as variable name
- Files > 500 lines must be split into directory modules
- All pub items must have doc comments
- TigerStyle: bounded mailboxes, pre/post assertions, budgets not limits

## Testing
```bash
cargo test --workspace              # Full workspace (430+ tests)
cargo test -p temper-server         # Including multi-tenant integration
cargo test -p temper-platform       # Platform unit + deploy pipeline
cargo test -p temper-platform --test platform_e2e_dst  # E2E shared registry proof
```

## Development Harness

See `docs/HARNESS.md` for the full harness reference with diagrams.

### Automated Enforcement (Claude Code Hooks)
- **Plan Reminder** (advisory): Reminds to create `.progress/` plan before edits
- **Spec Verification** (BLOCKING): L0-L3 cascade on every `.ioa.toml` edit
- **Dependency Isolation** (BLOCKING): Prevents temper-jit from pulling verify deps
- **Determinism Guard** (advisory): Warns about HashMap/SystemTime in simulation code
- **Post-Push Verify** (advisory): Runs tests after push, writes markers
- **Session Exit Gate** (BLOCKING): Blocks exit if unverified pushes or compile errors

### Git Hooks (installed via `scripts/setup-hooks.sh`)
- **Pre-commit**: Integrity check (no TODO/unwrap), spec syntax, dep audit
- **Pre-push**: 3-gate pipeline — integrity check, determinism audit, full test suite

Everything is automated. The harness runs on edit, commit, push, and session exit. No manual scripts to remember.

## Error Handling Standards (TigerStyle)
- **Bounded mailboxes**: Every actor mailbox has a capacity limit
- **Pre-assertions**: Validate inputs at function entry (`assert!` or `debug_assert!`)
- **Post-assertions**: Validate outputs before return
- **Budgets not limits**: Express constraints as budgets that get consumed, not arbitrary limits
- **Fail fast**: If an invariant is violated, panic immediately rather than propagating corrupt state
- **No silent failures**: Every error path must be logged or propagated

## Deployment Verification Steps
Before deploying any spec change:
1. Spec passes all 5 verification cascade levels (L0-L3)
2. TransitionTable builds successfully from verified spec
3. Entity actors hot-deploy without dropping existing state
4. OData endpoints respond correctly for all entity types
5. Telemetry emits WideEvents for all transitions
