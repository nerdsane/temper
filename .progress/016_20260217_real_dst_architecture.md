# Plan 016: Real DST Architecture — Canary + Comprehensive Reviewer

**Created**: 2026-02-17
**Status**: COMPLETE

## Execution Order (per plan)
1. Phase 2 — Rewrite DST reviewer agent
2. Phase 1 — Determinism canary tests
3. Phase 3 — Unify message handling path
4. Phase 4 — Improve fault injection usage
5. Phase 5 — Expand invariant vocabulary

## Parallelization Strategy
- Phase 2 (markdown rewrite) + Phase 1 (test code) + Phase 5 (invariant parsing) = independent, ran in parallel
- Phase 3 (code path unification) = independent of above, ran in parallel
- Phase 4 (fault injection defaults) = depends on Phase 1, ran after

## Phase Status

### Phase 2: Rewrite DST Reviewer Agent — DONE
- [x] Full rewrite of `.claude/agents/dst-reviewer.md`
- [x] Shift from pattern matching to holistic DST quality assessment
- [x] New output format: DST Maturity Assessment with 6 assessment areas

### Phase 1: Determinism Canary — DONE
- [x] Comprehensive canary test `determinism_canary_comprehensive` (6 seeds x 3 fault configs)
- [x] `determinism_canary_different_seeds_differ` test
- [x] Added `RunRecord` struct to SimActorSystem for full trace capture
- [x] Added `run_random_recorded()` method returning (SimActorResult, RunRecord)
- [x] Added canary check (Check 6) to pre-commit gate

### Phase 3: Unify Message Handling Path — DONE
- [x] Extracted `build_eval_context()` to `effects.rs` (single source of truth)
- [x] Extracted `process_action()` to `effects.rs` with `ProcessResult` return type
- [x] Production actor calls `process_action()` (thin wrapper with persistence + telemetry)
- [x] Simulation handler calls `process_action()` (thin sync wrapper)
- [x] Replay uses `build_eval_context()` (special case for stored events)

### Phase 4: Improve Fault Injection — DONE
- [x] Default SimActorSystemConfig now uses `FaultConfig::light()` (was `none()`)
- [x] Added per-entity-type heavy fault tests (5 new tests)
- [x] Added multi-seed heavy fault sweep (5 seeds)
- [x] Added multi-seed light fault sweep (5 seeds)
- [x] 16/33 tests use fault injection (~48%) — close to 50% target

### Phase 5: Expand Invariant Vocabulary — DONE
- [x] `OrderingConstraint` — `ordering(A, B)` checks state A precedes state B in event history
- [x] `NeverState` — `never(StateName)` asserts entity never reaches a forbidden state
- [x] `CounterCompare` — generalized `var >= N`, `var < N`, etc. (replaces CrossEntityRef)
- [x] `CompareOp` enum: Gt, Gte, Lt, Lte, Eq
- [x] All new variants handled in SimActorSystem::check_invariants()
- [x] Parser in sim_handler.rs supports all new patterns

## Verification
- `cargo test --workspace` — all tests pass (430+ total)
- `cargo clippy --workspace -- -D warnings` — clean
- `cargo check --workspace` — clean
- 33 DST tests in system_entity_dst.rs
- 811 lines added, 320 lines removed across 10 files
