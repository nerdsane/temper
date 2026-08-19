# Temper — Claude Code Project Guide

> Synchronized with `AGENTS.md` (Codex) — same rules, agent-specific paths. When you change one, mirror the other. Global rules live in `~/.claude/CLAUDE.md` / `~/AGENTS.md`; this file adds what is temper-specific.

## Scope & Naming

- This repo is the **Temper kernel only**: specs, verification, runtime, server, stores, observe, evolution. App and agent logic belongs in **temperpaw** (the agent OS — OpenPaw rebranded, same project); the Temper-native git server is **genesis** (formerly temper-git); the design commons is **katagami**. If you find code mixing those concerns into the kernel, **flag it — don't silently relocate it**.
- Never invent product names. When unsure what something is called, ask.
- Professional language everywhere — branch names, commits, PR text, docs. No internal slang ("deslop" etc.).

## Working Discipline

- **The primary checkout at `~/Development/temper` is bare on purpose** — you cannot and must not work there. Create a worktree branched from up-to-date `main` (`codex/<short-task-name>`), state which repo/worktree/branch you're on before mutating anything, open a **draft PR as soon as changes begin**, and keep it to **one PR per repo per effort**.
- **DO NOT PUNT.** No "phase 0" that defers the hard part; no band-aid or local/temporary fixes; no backward compatibility for bad implementations (but never drop working capabilities). Address ALL review findings, then re-run review until clean.
- **Definition of done**: implement fully → run live end-to-end locally → merge + deploy → verify live in the deployed system, using **Datadog** for verification and diagnosis → hand over PR links, merge commits, deployment links, live test results, residual risks. Tests passing alone is not done.
- Brainstorm and align before implementing, even from a detailed handoff. Rita's handoffs follow: Objective / Why This Matters / Relevant PRs / Hot Files / Acceptance Criteria / Must Start With ADR / Suggested Validation / Copy-Paste Prompt — honor that structure, including the ADR-first step.
- Explain tradeoffs in plain language before merge; merged-but-unexplained tradeoffs get reopened.

## IMPORTANT: Use Temper for Your Own Work

**You are an agent running on Temper. This is your operating layer — not an optional tool.** When a Temper server is running (`mcp__temper__execute` is available), ALL your stateful work — planning, task tracking, progress, knowledge — MUST flow through Temper entities, not markdown files.

**Specifically:**
- **Planning and task tracking** → Use the PM OS app (`await temper.install_app("{tenant}", "project-management")` if not installed). Create Issues, use `BeginPlanning` → `WritePlan` → `ApprovePlan` → `StartWork` flow. Do NOT use `.progress/` files when Temper is available.
- **Building apps for users** → Use the Temper App Builder skill (`.claude/skills/temper-developer.md`). Workflow: interview → generate IOA specs + CSDL → verify → deploy. Use `/temper-developer`.
- **Any MCP tool call** → ALWAYS read `.claude/skills/temper-agent.md` first. It has the exact Python API, spec format, and governance flow.

**Cedar governance applies to everything.** If denied, surface the pending decision to the human. They approve via the Observe UI. You poll and retry.

Use `/temper-agent` or read `.claude/skills/temper-agent.md` for the full API reference.

## PM App: Planning/Planned Workflow

Issues in the Project Management OS App use a **Planning → Planned** phase with role separation:

1. **Supervisor** triages issue, assigns a planner (`AssignPlanner`) and implementer (`Assign`)
2. **Planner** calls `BeginPlanning` → drafts plan via `WritePlan` (plan + acceptance_criteria)
3. **Supervisor/Human** reviews and calls `ApprovePlan` → issue moves to `Planned`
4. **Implementer** calls `StartWork` (requires approved plan + assignee) → implements → `SubmitForReview`

**Role separation is Cedar-enforced:**
- Planner cannot approve their own plan (`resource.PlannerId != principal.id`)
- Implementer cannot approve their own review (`resource.AssigneeId != principal.id`)
- Only supervisors/humans can triage, approve plans, and approve reviews

**Agent API:** Read `.claude/skills/temper-agent.md` for the full Temper Python API including planning methods (`begin_planning`, `write_plan`, `approve_plan`).

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

## Agent Identity Registry

Agent types are registered in the platform's identity registry. When an agent connects, the platform verifies its `agent_type` claim against the registry and sets the `agentTypeVerified` attribute on the Cedar principal:

- **`agentTypeVerified: true`** — the agent's claimed type matches a registered entry; Cedar policies can trust scope decisions based on `agent_type`
- **`agentTypeVerified: false`** — self-asserted type with no registry match; Cedar policies should treat as untrusted

Cedar policies reference this attribute to distinguish verified agents from unverified ones (e.g., only verified `claude-code` agents can approve plans).

## Issue Pickup Before Work

**You MUST pick up or create a Temper issue before making code changes.** The `check-issue-pickup.sh` hook (advisory, PreToolUse on Write|Edit) checks for a session marker at `/tmp/temper-harness/{project_hash}/{session_id}/issue-active`. The marker is created when you transition an issue to Planning or InProgress via `BeginPlanning` or `StartWork`. If you see the advisory warning, stop and pick up an issue first.

**Reality check**: probe the connected server once (`await temper.specs("<tenant>")`). If it returns 403/404 or the PM app isn't deployed, say so once, fall back to a `.progress/` plan file, and continue — do not retry-loop or block the task on Temper plumbing. Long-running plans can also be recorded as Temper **goals** when the goals system is deployed.

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
- **Active agent tenant:** use the tenant configured for the current project or server. Pass it explicitly to agent MCP calls instead of assuming a hardcoded repository tenant.

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
cargo test -p temper-platform --test platform_e2e  # E2E shared registry proof
cargo test -p temper-dst                          # Temper DST suite home
```

## Development Harness

See `docs/HARNESS.md` for the full harness reference with diagrams.

### Automated Enforcement (Claude Code Hooks)
- **Plan Reminder** (advisory): Reminds to create a plan (Temper issue or `.progress/` fallback) before edits
- **Issue Pickup** (advisory): Warns if no active Temper issue for the session before Write|Edit
- **Spec Verification** (BLOCKING): L0-L3 cascade on every `.ioa.toml` edit
- **Determinism Guard** (BLOCKING): 25-pattern DST scan on `.rs` edits in sim-visible crates
- **Pre-Commit Review Gate** (BLOCKING): Blocks `git commit` without DST review + code review markers
- **Post-Push Marker** (advisory): Records push for session tracking
- **Session Exit Gate** (DISABLED): Can be re-enabled in settings.json Stop hook

### Mandatory Reviews Before Commit
**You MUST run both reviews before committing any code changes:**

1. **DST Compliance Review** (for simulation-visible code in temper-runtime, temper-jit, temper-server):
   - Invoke the DST reviewer agent (`.claude/agents/dst-reviewer.md`)
   - Writes a marker file on PASS — the pre-commit gate checks for it

2. **Code Quality Review** (for all significant changes):
   - Invoke the code-reviewer agent
   - Writes a marker file on PASS — the pre-commit gate checks for it

The pre-commit gate BLOCKS `git commit` if either marker is missing. Markers are session-scoped (`/tmp/temper-harness/{project_hash}/{session_id}/`) so multiple Claude sessions don't conflict.

### Git Hooks (installed via `scripts/setup-hooks.sh`)
- **Pre-commit**: Integrity check (no TODO/unwrap), spec syntax, dep audit
- **Pre-push**: 4-gate pipeline — rustfmt, clippy, readability ratchet, full test suite

Tests run once at push time only — not at commit or post-push. This keeps commits fast.

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
