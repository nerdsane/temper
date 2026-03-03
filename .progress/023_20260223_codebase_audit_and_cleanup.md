# 023 — Codebase Audit & Cleanup

> Date: 2026-02-23
> Goal: Understand entirety, clean up bloat, eliminate gaps/inconsistencies, make presentable

## Phase 1: Assessment (COMPLETE)

### Compilation Status: CLEAN
- Initial test run showed 4 errors — caused by stale incremental build cache
- Fresh `cargo check --workspace` and `cargo test --workspace` both pass with **0 errors, 0 warnings**
- The methods `upsert_tenant_constraints` / `delete_tenant_constraints` DO exist in TursoEventStore (lines 278-311)
- All ~311 tests pass across all crates

### Bloat Identified

#### Files to Delete
1. `generated_imgs/` — 11 AI-generated PNGs, not source code, not gitignored
2. `temper-demo/` — contains only `node_modules/`, zero source files
3. `TEMPER_BUILDER.skill.md` — duplicate of `.claude/skills/temper.md`
4. `TEMPER_USER.skill.md` — duplicate of `.claude/skills/temper-user.md`
5. `.z3-trace` — debugging artifact (already in .gitignore)
6. `.progress/` — 18 deleted but untracked plan files showing in git status

#### Files Violating 500-Line Rule (.vision/CONSTRAINTS.md)
1. `temper-server/src/observe_routes.rs` — **4,121 lines** (8x limit)
2. `temper-cli/src/serve/mod.rs` — **1,228 lines** (2.5x limit)
3. `temper-mcp/src/lib.rs` — **1,153 lines** (2.3x limit)
4. `temper-server/src/dispatch.rs` — **918 lines** (1.8x limit)
5. `temper-server/src/state.rs` — **1,211 lines** (2.4x limit)
6. `temper-spec/src/automaton/parser.rs` — **753 lines** (1.5x limit)
7. `temper-spec/src/csdl/parser.rs` — **676 lines** (1.4x limit)
8. `temper-spec/src/tlaplus/extractor.rs` — **664 lines** (1.3x limit)
9. `temper-runtime/src/scheduler/mod.rs` — **650 lines** (1.3x limit)
10. `temper-store-redis/src/event_store.rs` — **605 lines** (1.2x limit)
11. `temper-odata/src/types.rs` — **599 lines** (1.2x limit)
12. `temper-verify/src/simulation.rs` — **545 lines** (1.1x limit)
13. `temper-verify/src/smt.rs` — **565 lines** (1.1x limit)

#### Duplicate/Redundant Documentation
- `docs/PAPER.md` (73KB) — Research paper, massive
- `docs/AGENT_GUIDE.md` (54KB) — Agentic development guide
- `docs/AUDIT.md` (26KB) — Previous audit results, may be stale
- `docs/USE_CASES.md` (29KB) — Use case analysis
- Multiple overlap between docs/, CLAUDE.md, .vision/, CODING_GUIDELINES.md, README.md

### Gaps & Inconsistencies

#### Compilation Gaps
1. **Missing Turso methods** — `state.rs` calls `upsert_tenant_constraints` / `delete_tenant_constraints` on TursoEventStore but they're not implemented

#### Architectural Inconsistencies
1. **Cargo.toml says rust-version 1.85** but CONSTRAINTS.md also says 1.85, while actual workspace Cargo.toml may differ — need to verify consistency
2. **temper-macros** is a 30-line stub — either implement or remove from workspace
3. **temper-codegen** is partially stubbed — generator structure exists but output is limited
4. **temper-optimize** — framework present but tuning algorithms are basic/placeholder

#### Open P2 Gaps from GAP_TRACKER.md (14 remaining)
- #20: MaxCount guard never parsed
- #21: Hand-rolled TOML parser fragility
- #22: Shadow testing uses legacy API only
- #24: No batch request support
- #25: No $search support
- #26: No $apply aggregation support
- #27: init template uses relative path
- #28: No graceful shutdown
- #29: Optimization recommendations not applied
- #30: No observability providers beyond ClickHouse
- #31: Legacy TLA+ extractor is brittle
- #32: Generated code not validated
- #34: Proc macros limited to marker traits
- #36: IncrementItems/DecrementItems legacy aliases

### Verification & Testing Summary

**Total Tests**: ~594 across workspace (can't run due to compile error)

**Verification Cascade** (temper-verify):
- L0: SMT symbolic verification via Z3 — checks guard satisfiability
- L1: Stateright exhaustive model checking — dead guards, unreachable states, inductive invariants
- L2: Deterministic simulation — fault injection, message drops/reordering/delays
- L2b: Multi-actor simulation — cross-entity consistency
- L3: Property-based tests via proptest — boundary values, edge cases

**Test Types Present**:
- Unit tests: inline `#[test]` in most modules
- Integration tests: `tests/` directories in temper-server, temper-platform
- E2E tests: platform_e2e_dst, system_entity_dst, compile_first_e2e
- Benchmark tests: agent_checkout, agent_triage in reference apps
- DST compliance: determinism guard hook runs on every edit

**Automated Enforcement** (hooks):
- Pre-edit: plan reminder, spec verification cascade, dep isolation, determinism guard
- Pre-commit: integrity check (no TODO/unwrap), spec syntax, dep audit
- Pre-push: 3-gate pipeline (integrity, determinism, full test suite)
- Session exit: unverified push check

---

## Phase 2: Fix Compilation (Priority 1) — NOT NEEDED

- [x] Compilation is clean — initial errors were from stale incremental cache
- [x] `cargo test --workspace` passes: 311 tests, 0 failures, 0 warnings

## Phase 3: Remove Bloat (Priority 2)

- [ ] Delete `generated_imgs/` directory
- [ ] Delete `temper-demo/` directory (empty except node_modules)
- [ ] Delete `TEMPER_BUILDER.skill.md` (duplicate)
- [ ] Delete `TEMPER_USER.skill.md` (duplicate)
- [ ] Delete `.z3-trace` if present
- [ ] Add `generated_imgs/`, `temper-demo/` to .gitignore
- [ ] Clean up stale `.progress/` files from git status

## Phase 4: File Size Violations (Priority 3)

- [ ] Split `observe_routes.rs` (4,121 lines) into directory module
- [ ] Split `state.rs` (1,211 lines) into directory module
- [ ] Split `serve/mod.rs` (1,228 lines) into submodules
- [ ] Split `temper-mcp/lib.rs` (1,153 lines) into modules
- [ ] Split `dispatch.rs` (918 lines) into submodules

## Phase 5: Consistency & Presentability (Priority 4)

- [ ] Evaluate temper-macros — stub or remove
- [ ] Clean up duplicate docs surface (reconcile docs/, CLAUDE.md, README.md overlap)
- [ ] Ensure README.md is presentable for external viewing
- [ ] Verify all `pub` items have doc comments in core crates

---

## Instance Log
| Instance | Phase | Status |
|----------|-------|--------|
| Main | Assessment | COMPLETE |
