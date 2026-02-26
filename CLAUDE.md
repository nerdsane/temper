# Temper — Claude Code Project Guide

## IMPORTANT: Building Apps

**When the user asks to build an app, create an app, or says "build me a X" — ALWAYS use the Temper App Builder skill (`.claude/skills/temper-developer.md`).** Do NOT treat this as a generic web development request. Do NOT use brainstorming, frontend-design, or other general-purpose skills for app creation.

Temper builds apps from specs, not from code. The workflow is: interview → generate IOA specs + CSDL → verify → deploy. Follow the skill's Interview Protocol.

Use `/temper-developer` or read `.claude/skills/temper-developer.md` and follow it step by step.

## IMPORTANT: Using Temper MCP Tools

**When calling `mcp__temper__execute` or `mcp__temper__search` — ALWAYS read `.claude/skills/temper-agent.md` first.** It has the exact Python API, spec format, and governance flow. Without it you will get spec parse errors and method signature mistakes.

Use `/temper-agent` or read `.claude/skills/temper-agent.md` and follow its patterns.

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

## Architecture Decision Records (ADRs)

**Every significant implementation MUST start with an ADR as the first step.** Before writing any code, create `docs/adrs/NNNN-short-title.md` following the template at `docs/adrs/TEMPLATE.md`. Required for new features, architectural changes, new integrations, multi-crate changes, or new patterns. Not required for bug fixes, single-file refactors, doc changes, or test additions.

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

### Deterministic Simulation (FoundationDB/TigerBeetle Standards)
In simulation-visible crates (temper-runtime, temper-jit, temper-server):
- Use `sim_now()` / `sim_uuid()` instead of wall clock / random UUIDs
- Use `BTreeMap`/`BTreeSet` not `HashMap`/`HashSet` — deterministic iteration order
- No `std::thread::spawn`, `rayon`, or multi-threaded `tokio::spawn` — single-threaded actor model
- No `std::fs`, `std::net`, `std::env::var` — abstract all I/O behind traits
- No `static mut`, `lazy_static!`, `thread_local!` — pass state through actor context
- No `chrono::Utc::now()`, `std::thread::sleep()` — use simulated time
- No `OsRng`, `getrandom` — use seeded PRNG
- `SimActorHandler::spec_invariants()` auto-checks [[invariant]] sections
- Add `// determinism-ok` to suppress false positives in the determinism guard
- See `.claude/agents/dst-reviewer.md` for the full DST compliance ruleset

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
- **Determinism Guard** (BLOCKING): 25-pattern DST scan based on FoundationDB/TigerBeetle practices
- **Pre-Commit Review Gate** (BLOCKING): Blocks `git commit` without DST review + code review + passing tests
- **Post-Push Verify** (advisory): Runs tests after push, writes markers
- **Session Exit Gate** (BLOCKING): Blocks exit if unverified pushes, missing reviews, or compile errors

### Mandatory Reviews Before Commit
**You MUST run both reviews before committing any code changes:**

1. **DST Compliance Review** (for simulation-visible code in temper-runtime, temper-jit, temper-server):
   - Invoke the DST reviewer agent (`.claude/agents/dst-reviewer.md`)
   - Reviews code for determinism violations beyond pattern matching
   - Writes a marker file on PASS — the pre-commit gate checks for it

2. **Code Quality Review** (for all significant changes):
   - Invoke the code-reviewer agent
   - Reviews against plan, coding standards, TigerStyle
   - Writes a marker file on PASS — the pre-commit gate checks for it

The pre-commit gate BLOCKS `git commit` if either marker is missing. The session exit gate is a safety net that catches anything that slips through.

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
