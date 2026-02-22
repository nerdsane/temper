# Temper Development Harness

The Temper harness is a multi-layered enforcement system that catches problems at every stage — from editing to committing to pushing to session exit. Nothing gets through without verification.

```
┌─────────────────────────────────────────────────────────────────────┐
│                                                                     │
│   ╔═══════════╗     ╔═══════════╗     ╔═══════════╗     ╔════════╗ │
│   ║  EDIT     ║────▶║  COMMIT   ║────▶║  PUSH     ║────▶║  EXIT  ║ │
│   ╚═══════════╝     ╚═══════════╝     ╚═══════════╝     ╚════════╝ │
│       │                  │                  │                 │      │
│       ▼                  ▼                  ▼                 ▼      │
│   ┌────────┐        ┌────────┐        ┌────────┐       ┌────────┐  │
│   │Spec    │        │Review  │        │Post-   │       │Exit    │  │
│   │Verify  │        │Gate    │        │Push    │       │Gate    │  │
│   │Dep Iso │        │Tests   │        │Tests   │       │Reviews │  │
│   │DST Scan│        │DST Rev │        │Markers │       │Compile │  │
│   │Plan    │        │Code Rev│        │        │       │Markers │  │
│   └────────┘        └────────┘        └────────┘       └────────┘  │
│    BLOCKING          BLOCKING          advisory         BLOCKING    │
│                                                                     │
│                     Claude Code Hooks                               │
│                                                                     │
│   ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─  │
│                                                                     │
│   ┌─────────────────────┐     ┌─────────────────────┐              │
│   │ Git Pre-Commit      │     │ Git Pre-Push        │              │
│   │ • Integrity check   │     │ • Integrity check   │              │
│   │ • Spec syntax       │     │ • Determinism audit │              │
│   │ • Dep audit         │     │ • Full test suite   │              │
│   └─────────────────────┘     └─────────────────────┘              │
│   ┌─────────────────────┐                                           │
│   │ Git Post-Commit     │                                           │
│   │ • commit-pending    │                                           │
│   │ • sim-changed       │                                           │
│   └─────────────────────┘                                           │
│    BLOCKING (git hooks)        BLOCKING (git hooks)                 │
│                                                                     │
│   ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─  │
│                                                                     │
│   ┌─────────────────────────────────────────────────────────────┐  │
│   │ CI (GitHub Actions) — cannot be bypassed                    │  │
│   │ • temper verify    • cargo test    • dependency audit        │  │
│   └─────────────────────────────────────────────────────────────┘  │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

## Two Enforcement Contexts

This harness enforces quality for **agents developing Temper itself** (the framework). For **agents building apps WITH Temper**, enforcement happens differently:

- `temper serve --specs-dir specs/` runs the full verification cascade at startup and rejects broken specs
- The Temper skill file (`.claude/skills/temper.md`) guides agents to run `temper verify` after writing specs
- `temper init` scaffolds hooks into new projects for local enforcement
- Temper itself IS the enforcement layer for apps — no separate harness needed

---

## All Components at a Glance

```
                        ┌──────────────┐
                        │  BLOCKING?   │
                        └──────┬───────┘
                               │
          ┌────────────────────┼────────────────────┐
          │                    │                    │
     ╔════╧═════╗        ╔════╧═════╗        ╔════╧═════╗
     ║ BLOCKING ║        ║ ADVISORY ║        ║ AGENT    ║
     ║ (exit 2) ║        ║ (exit 0) ║        ║ REVIEW   ║
     ╚════╤═════╝        ╚════╤═════╝        ╚════╤═════╝
          │                    │                    │
    ┌─────┴──────┐      ┌─────┴──────┐      ┌─────┴──────┐
    │Spec Verify │      │Plan Remind │      │DST Review  │
    │Dep Isolate │      │Post-Push   │      │Code Review │
    │DST Scan    │      │Verify      │      │            │
    │Review Gate │      └────────────┘      └────────────┘
    │Exit Gate   │       warns, you          mandatory,
    │Integrity   │       decide              writes markers
    │Spec Syntax │                           checked by
    │Dep Audit   │                           Review Gate +
    │Full Tests  │                           Exit Gate
    └────────────┘
     stops you,
     must fix
```

| # | Component | Type | Layer | Trigger | Blocking |
|---|-----------|------|-------|---------|----------|
| 1 | Plan Reminder | Shell hook | Claude Code PreToolUse | Write/Edit | No |
| 2 | Spec Verification | Shell hook | Claude Code PostToolUse | Write/Edit on spec files | **YES** |
| 3 | Dependency Isolation | Shell hook | Claude Code PostToolUse | Write/Edit on Cargo.toml | **YES** |
| 4 | Determinism Guard | Shell hook | Claude Code PostToolUse | Write/Edit on .rs in sim crates | **YES** |
| 5 | Pre-Commit Review Gate | Shell hook | Claude Code PreToolUse | Bash `git commit` | **YES** |
| 6 | Post-Push Verify | Shell hook | Claude Code PostToolUse | Bash `git push` | No (creates markers) |
| 7 | Session Exit Gate | Shell hook | Claude Code Stop | Session end | **YES** |
| 8 | DST Compliance Review | Agent | Claude Code (manual invoke) | Before commit (mandatory) | **YES** (via markers) |
| 9 | Code Quality Review | Agent | Claude Code (manual invoke) | Before commit (mandatory) | **YES** (via markers) |
| 10 | Trace Capture | Shell hook | Claude Code PostToolUse | All tools (`.*`) | No (advisory) |
| 11 | Alignment Review | Agent | Claude Code (manual invoke) | Before commit (mandatory) | **YES** (via markers) |
| 12 | Integrity Check | Git hook | pre-commit | `git commit` | **YES** |
| 13 | Spec Syntax | Git hook | pre-commit | `git commit` | **YES** |
| 14 | Dep Audit | Git hook | pre-commit | `git commit` | **YES** |
| 15 | Full Test Suite | Git hook | pre-push | `git push` | **YES** |
| 16 | Commit Marker Writer | Git hook | post-commit | `git commit` | **YES** (for stop-gate wiring) |

---

## Tier 1: Claude Code Hooks (Design-Time)

Configured in `.claude/settings.json`. Fire automatically during Claude Code sessions.

### Component 1: Plan Reminder

```
File:      .claude/hooks/check-plan-reminder.sh
Trigger:   PreToolUse — Write|Edit
Blocking:  No
```

Before any file edit, checks if a `.progress/` plan exists. Displays a reminder to create one if missing. Keeps planning discipline without hard-blocking.

### Component 2: Spec Verification Gate

```
File:      .claude/hooks/verify-specs.sh
Trigger:   PostToolUse — Write|Edit (on .ioa.toml, .csdl.xml, .cedar files)
Blocking:  YES (exit 2)
```

The core of Temper's value proposition. After editing any spec file, runs the full verification cascade:

```
  ┌──────────────────────────────────────────────────────────┐
  │                 Spec Verification Flow                    │
  │                                                          │
  │   Edit .ioa.toml                                         │
  │        │                                                 │
  │        ▼                                                 │
  │   ┌─────────┐   ┌─────────┐   ┌─────────┐   ┌────────┐│
  │   │ L0 SMT  │──▶│ L1 Model│──▶│ L2 DST  │──▶│L3 Prop ││
  │   │ Z3      │   │ Check   │   │ Sim     │   │Test    ││
  │   │         │   │ BFS     │   │ Faults  │   │Random  ││
  │   └────┬────┘   └────┬────┘   └────┬────┘   └───┬────┘│
  │        │              │              │             │     │
  │        ▼              ▼              ▼             ▼     │
  │   Guard SAT?    All states    Fault-tolerant?  Random   │
  │   Invariants    explored?     Messages OK?     sequences│
  │   inductive?    Properties    Multi-actor?     hold?    │
  │                 hold?                                    │
  │                                                          │
  │   ALL FOUR must pass ──▶ Edit allowed                    │
  │   ANY failure ──▶ Edit BLOCKED                           │
  └──────────────────────────────────────────────────────────┘
```

**L0 — SMT/Z3**: Algebraic proof. Checks guard satisfiability (are any guards dead code?), invariant induction (does each invariant hold across transitions?), and unreachable state detection. No state exploration — pure logic.

**L1 — Model Checking (Stateright)**: Exhaustive BFS of every reachable state. Checks all `[[invariant]]` and `[[liveness]]` properties. If it's reachable and it breaks something, L1 finds it. Returns counterexample traces.

**L2 — Deterministic Simulation**: Multi-actor simulation with fault injection (message delays, drops, crashes). 5 seeds, deterministic via `sim_now()`/`sim_uuid()`. Tests that the system works under distributed chaos.

**L3 — Property Tests**: Random action sequences (100 cases, 30 steps each) on the abstract model. Redundant with L1 for small specs — serves as a fast sanity check and fallback when L1's state space is too large to complete.

### Component 3: Dependency Isolation Guard

```
File:      .claude/hooks/check-deps.sh
Trigger:   PostToolUse — Write|Edit (on Cargo.toml files)
Blocking:  YES (exit 2)
```

After editing any `Cargo.toml`, checks the real dependency graph via `cargo tree`:

```
  ┌─────────────────────────────────────────────────────────┐
  │              Dependency Firewall                         │
  │                                                         │
  │   PRODUCTION                    VERIFICATION            │
  │   ┌─────────────┐              ┌──────────────┐        │
  │   │ temper-jit   │──── ✗ ────▶│ temper-verify │        │
  │   │ temper-server│              │   stateright │        │
  │   │ temper-       │              │   proptest   │        │
  │   │   runtime    │              │   z3         │        │
  │   └─────────────┘              └──────────────┘        │
  │                                                         │
  │   Production crates must NEVER depend on verification   │
  │   crates. This keeps binaries small and fast.           │
  └─────────────────────────────────────────────────────────┘
```

### Component 4: Determinism Guard (DST Pattern Scan)

```
File:      .claude/hooks/check-determinism.sh
Trigger:   PostToolUse — Write|Edit (on .rs files in sim-visible crates)
Blocking:  YES (exit 2)
```

After editing Rust files in `temper-runtime`, `temper-jit`, or `temper-server`, scans for 24 non-deterministic patterns based on FoundationDB, TigerBeetle, S2, and Polar Signals DST practices:

| Category | Banned Patterns | Use Instead |
|----------|----------------|-------------|
| **Collections** | `HashMap`, `HashSet`, `DashMap`, `FuturesUnordered`, `IndexMap` | `BTreeMap`, `BTreeSet`, deterministic ordering |
| **Time** | `SystemTime::now`, `Instant::now`, `chrono::Utc::now`, `thread::sleep`, `tokio::time::sleep` | `sim_now()`, simulated time |
| **Randomness** | `Uuid::new_v4`, `thread_rng`, `rand::random`, `OsRng`, `getrandom` | `sim_uuid()`, seeded PRNG |
| **Threading** | `std::thread::spawn`, `rayon::*`, `tokio::spawn` | Actor model, message passing |
| **I/O** | `std::fs::*`, `std::net::*`, `std::env::var`, `std::process::id` | Trait-abstracted I/O, simulation context |
| **Global State** | `static mut`, `lazy_static!`, `thread_local!` | Actor context, explicit parameters |
| **Serialization** | `sort_unstable` | `sort()` (stable) |

Suppress false positives with `// determinism-ok` on the line.

**This is the fast gate.** It catches obvious violations instantly. The DST reviewer agent (Component 8) catches semantic violations that pattern matching cannot.

### Component 5: Pre-Commit Review Gate

```
File:      .claude/hooks/pre-commit-review-gate.sh
Trigger:   PreToolUse — Bash (on commands containing `git commit`)
Blocking:  YES (exit 2)
```

Before any `git commit` command executes, checks three things:

```
  ┌───────────────────────────────────────────────────────────┐
  │               Pre-Commit Review Gate                       │
  │                                                           │
  │   git commit                                              │
  │       │                                                   │
  │       ▼                                                   │
  │   ┌──────────────────────┐                                │
  │   │ Sim-visible code     │──── yes ───▶ DST review       │
  │   │ changed?             │              marker exists?    │
  │   └──────────────────────┘              │                 │
  │                                    no ──┤                 │
  │                                         ▼                 │
  │                                    ╔═══════════╗          │
  │                                    ║  BLOCKED  ║          │
  │                                    ╚═══════════╝          │
  │                                                           │
  │   ┌──────────────────────┐                                │
  │   │ Code review marker   │──── no ────▶ BLOCKED           │
  │   │ exists?              │                                │
  │   └──────────────────────┘                                │
  │                                                           │
  │   ┌──────────────────────┐                                │
  │   │ cargo test           │──── fail ──▶ BLOCKED           │
  │   │ --workspace          │                                │
  │   └──────────────────────┘                                │
  │                                                           │
  │   All three pass ──▶ Commit proceeds                      │
  └───────────────────────────────────────────────────────────┘
```

This is the **primary enforcement point** for mandatory reviews. The session exit gate (Component 7) is the safety net.

### Component 6: Post-Push Verification

```
File:      .claude/hooks/post-push-verify.sh
Trigger:   PostToolUse — Bash (on commands containing `git push`)
Blocking:  No (but creates markers for Component 7)
```

After any `git push`:
1. Writes `push-pending-{session}` marker
2. Runs `cargo test --workspace`
3. On pass: writes `test-verified-{session}` marker
4. On fail: marker remains unverified — Component 7 blocks session exit

```
  push ──▶ push-pending marker ──▶ cargo test ──▶ test-verified marker
                                        │
                                   fail │
                                        ▼
                              Session exit BLOCKED
                              (Component 7 catches this)
```

### Component 7: Session Exit Gate

```
File:      .claude/hooks/stop-verify.sh
Trigger:   Stop (session end)
Blocking:  YES (exit 2)
```

Before Claude Code session ends, checks four things:

1. **Unverified pushes**: `push-pending` marker without `test-verified` marker?
2. **Missing DST review**: Sim-visible code committed without `dst-reviewed` marker?
3. **Missing code review**: Code committed without `code-reviewed` marker?
4. **Compilation**: `cargo check --workspace` passes?

This is the **safety net**. Even if the pre-commit gate somehow didn't catch a missing review, you cannot leave the session without resolving it.

---

## Tier 1b: Agent Reviews (Mandatory)

These are Claude Code sub-agents that perform semantic code review. They're mandatory — the pre-commit gate and session exit gate check for their markers.

### Component 8: DST Compliance Review

```
File:      .claude/agents/dst-reviewer.md
Trigger:   Manual invoke (mandatory before committing sim-visible code)
Blocking:  YES (via marker — pre-commit gate + exit gate check for it)
```

A specialized review agent that performs **semantic analysis** of simulation-visible code changes. Goes beyond pattern matching:

```
  ┌──────────────────────────────────────────────────────────────┐
  │              DST Compliance Review                            │
  │                                                              │
  │   Pattern matching          Semantic analysis                │
  │   (Component 4)             (Component 8)                    │
  │                                                              │
  │   "HashMap found"           "BTreeMap is correct, but it's   │
  │                              populated from a function that  │
  │                              internally uses HashSet —       │
  │                              non-deterministic data flow"    │
  │                                                              │
  │   "Instant::now found"      "sim_now() is used, but the     │
  │                              closure captures a reference    │
  │                              to a struct that caches real    │
  │                              timestamps from its constructor"│
  │                                                              │
  │   Catches 24 patterns       Catches data flow, ordering     │
  │   (~2ms)                    deps, hidden non-determinism     │
  │                             (~30-60s)                        │
  └──────────────────────────────────────────────────────────────┘
```

What it reviews:
- **Data flow**: Where does each value come from? Is the source deterministic?
- **Ordering**: Are sort keys total orders? Do iterators preserve deterministic order?
- **Concurrency**: Are messages processed in FIFO order? Can async points race?
- **Hidden state**: Do trait impls have non-deterministic defaults? Do closures capture non-deterministic references?
- **Boundaries**: Where does sim-visible code call into non-sim code? Are boundaries clean?

On PASS, writes `dst-reviewed-{session}` marker. On FAIL, lists findings that must be resolved.

### Component 9: Code Quality Review

```
Trigger:   Manual invoke (mandatory before committing any code)
Blocking:  YES (via marker — pre-commit gate + exit gate check for it)
```

General code quality review using the code-reviewer agent. Reviews against:
- The current plan in `.progress/`
- Coding standards (TigerStyle, Rust conventions)
- Architectural alignment with Temper's vision

On PASS, writes `code-reviewed-{session}` marker.

---

## Tier 2: Git Hooks (Commit-Time)

Install with: `scripts/setup-hooks.sh`. Fire for anyone using git — humans, agents, CI.

### Component 10: Pre-Commit — Integrity Check

```
File:      .claude/hooks/pre-commit.sh (installed to .git/hooks/pre-commit)
Blocking:  YES
```

Scans staged `.rs` files (excluding tests) for:
- `TODO`, `FIXME`, `XXX`, `HACK` comments
- `unimplemented!()` / `todo!()` macros
- `panic!("not implemented")`
- `.unwrap()` calls

### Component 11: Pre-Commit — Spec Syntax Validation

```
Blocking:  YES (part of pre-commit hook)
```

If any staged file is `*.ioa.toml`, runs `temper verify` to check syntax. Catches spec errors from edits made outside Claude Code.

### Component 12: Pre-Commit — Dependency Audit

```
Blocking:  YES (part of pre-commit hook)
```

If any `Cargo.toml` was staged, runs `scripts/audit-deps.sh`. Same check as Component 3, but catches direct git commits that bypass Claude Code hooks.

### Component 13: Pre-Push — Full Test Suite

```
File:      .claude/hooks/pre-push.sh (installed to .git/hooks/pre-push)
Blocking:  YES
```

Runs a 3-gate pipeline before every push:

| Gate | What it checks | Blocking |
|------|---------------|----------|
| 1/3 | Integrity (no TODO/unwrap/hacks) | YES |
| 2/3 | Determinism patterns in sim crates | Advisory |
| 3/3 | `cargo test --workspace` | YES |

Bypass with `git push --no-verify` for emergencies only.

### Component 14: Post-Commit — Commit Marker Writer

```
File:      .claude/hooks/post-commit.sh (installed to .git/hooks/post-commit)
Blocking:  YES (for session exit safety-net wiring)
```

After every successful commit, writes:
- `commit-pending` marker (a commit happened this session)
- `sim-changed` marker if the commit touched `crates/temper-runtime`, `crates/temper-jit`, or `crates/temper-server` Rust files

These markers are consumed by the session exit gate to enforce review safety nets.

---

## Tier 3: CI (GitHub Actions)

The one layer that **cannot be bypassed**. Runs on every PR.

- `temper verify --specs-dir specs/` — full L0-L3 cascade
- `cargo test --workspace` — all tests
- Dependency isolation audit — no verify deps in production

Note: CI DST pattern scan is currently disabled due false positives (`.github/workflows/ci.yml`); semantic DST review is enforced via the review markers/gates.

---

## Marker System

The harness uses temporary marker files in `/tmp/temper-harness/{project_hash}/` to coordinate between hooks:

```
  ┌──────────────────────────────────────────────────────────────┐
  │                    Marker Flow                                │
  │                                                              │
  │   DST reviewer ────▶ dst-reviewed-{session}   ──┐           │
  │                                                  │           │
  │   Code reviewer ───▶ code-reviewed-{session}  ──┼──▶ Gate   │
  │                                                  │   checks  │
  │   git push ────────▶ push-pending-{session}   ──┤   these   │
  │                                                  │           │
  │   cargo test pass ─▶ test-verified-{session}  ──┘           │
  │                                                              │
  │   Checked by:                                                │
  │   • Pre-commit gate (Component 5) — before commit            │
  │   • Session exit gate (Component 7) — before session ends    │
  │                                                              │
  │   Cleaned up: on successful session exit                     │
  └──────────────────────────────────────────────────────────────┘
```

---

## Session Lifecycle

A typical development session flows through all enforcement layers:

```
  ┌─────────────────────────────────────────────────────────────────┐
  │                                                                 │
  │  1. SESSION START                                               │
  │     │                                                           │
  │     ▼                                                           │
  │  2. EDIT CODE                                                   │
  │     ├── .ioa.toml? ──▶ Spec Verification (L0-L3)  [BLOCKING]  │
  │     ├── Cargo.toml? ──▶ Dep Isolation Guard        [BLOCKING]  │
  │     ├── .rs in sim? ──▶ DST Pattern Scan (25 pat)  [BLOCKING]  │
  │     └── any file? ────▶ Plan Reminder              [advisory]  │
  │     │                                                           │
  │     ▼                                                           │
  │  3. REVIEW (mandatory before commit)                            │
  │     ├── Sim code changed? ──▶ DST Reviewer Agent               │
  │     │                         writes dst-reviewed marker        │
  │     └── Any code changed? ──▶ Code Reviewer Agent              │
  │                               writes code-reviewed marker      │
  │     │                                                           │
  │     ▼                                                           │
  │  4. COMMIT                                                      │
  │     ├── Claude Code: Pre-commit gate checks markers [BLOCKING] │
  │     │   + runs cargo test --workspace               [BLOCKING] │
  │     └── Git hook: integrity, spec syntax, dep audit [BLOCKING] │
  │     │                                                           │
  │     ▼                                                           │
  │  5. PUSH                                                        │
  │     ├── Git hook: 3-gate pipeline (integrity,       [BLOCKING] │
  │     │   determinism, full tests)                                │
  │     └── Claude Code: post-push writes markers,      [advisory] │
  │         runs tests, coordinates with exit gate                  │
  │     │                                                           │
  │     ▼                                                           │
  │  6. SESSION END                                                 │
  │     └── Exit gate checks:                           [BLOCKING] │
  │         • Unverified pushes?                                    │
  │         • Missing DST review?                                   │
  │         • Missing code review?                                  │
  │         • Compilation errors?                                   │
  │                                                                 │
  └─────────────────────────────────────────────────────────────────┘
```

---

## Who Catches What

```
  ┌───────────────────────────────────────────────────────────────┐
  │                    Enforcement Matrix                          │
  │                                                               │
  │                      Claude Code    Git Hooks    CI           │
  │                      ───────────    ─────────    ──           │
  │  Broken spec         Spec Verify    Spec Syntax  temper verify│
  │  Bad deps            Dep Isolate    Dep Audit    Dep audit    │
  │  HashMap in sim      DST Scan       Pre-push     —           │
  │  Semantic DST bug    DST Reviewer   —            —            │
  │  Code quality        Code Reviewer  —            —            │
  │  Tests failing       Review Gate    Pre-push     cargo test   │
  │  TODO/unwrap         —              Pre-commit   —            │
  │  Unverified push     Exit Gate      —            —            │
  │  Compile errors      Exit Gate      —            cargo check  │
  │                                                               │
  │  Can bypass?         NO             --no-verify  NO           │
  └───────────────────────────────────────────────────────────────┘
```

**Claude Code hooks** catch agents. **Git hooks** catch humans. **CI** catches everything.

---

## DST Pattern Reference

The determinism guard (Component 4) checks these patterns. Based on practices from:
- **FoundationDB**: `g_network->now()`, `deterministicRandom()`, single-threaded cooperative multitasking, `Net2`/`Sim2` interface swapping
- **TigerBeetle**: Zero external deps, static memory allocation, custom deterministic collections, `TimeSim` virtual time
- **S2**: `getrandom`/`getentropy` interception, single-threaded Tokio with `RngSeed`, paused time mode, determinism canary
- **Polar Signals**: `async` banned from state machine traits, synchronous state machines ticked by deterministic message bus

See `.claude/agents/dst-reviewer.md` for the full semantic review ruleset.

---

## Setup

```bash
# Install git hooks (one-time)
scripts/setup-hooks.sh

# Claude Code hooks are configured automatically via .claude/settings.json
# No manual setup needed — hooks fire on edit, commit, push, and exit
```

## Bypassing (Emergencies Only)

```bash
# Skip git pre-commit checks
git commit --no-verify

# Skip git pre-push test suite
git push --no-verify
```

Claude Code hooks **cannot be bypassed** — they're enforced by the tool itself. If a blocking hook fires, you must fix the issue. The `// determinism-ok` comment is the only escape hatch for false positives in the DST pattern scan.

---

## Trace + Marker Utilities

Trace capture remains part of the harness and runs for all tool calls:

- Trace hook: `.claude/hooks/trace-capture.sh`
- Trace verifier: `scripts/verify-trace.sh`
- Marker writer: `scripts/write-marker.sh`

All markers use TOML (`*.toml`) plus a backward-compatible plain marker file.

## Portability

The portable output contract for this harness is `verification.v1`.

- Schema: `docs/verification.v1.schema.json`
- Mapping + hardness baseline: `docs/verification.v1.mapping.md`
- Report generator: `scripts/verification-v1-report.sh`
- Contract validator: `scripts/verification-v1-validate.sh`

Generate a normalized report:

```bash
scripts/verification-v1-report.sh --pretty
```

This exports hook/gate/marker evidence in one model-agnostic JSON document that any runtime can consume.

CI now consumes this contract via job `verification-contract` in `.github/workflows/ci.yml`:

- Generates `verification.v1.json`
- Validates contract shape
- Enforces policy (`blocking_failures == 0` and `checks_failed == 0`)
- Uploads the report as build artifact
