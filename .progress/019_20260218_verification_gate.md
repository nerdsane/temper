# Plan: Verification Gate + Rich Failure Details

## Status: COMPLETE

## Phases

### Phase 1: Enrich Failure Details in Registry
- [x] Add `VerificationDetail` struct to `registry.rs`
- [x] Add `details` field to `EntityLevelSummary`
- [x] Update `CascadeResult → EntityLevelSummary` conversion in `observe_routes.rs`
- [x] Update `handle_verification_status` to include details in response

### Phase 2: Verification Gate on Runtime Operations
- [x] Add `VerificationGateError`, `FailedLevelInfo`, and `check_verification_gate()` to `state.rs`
- [x] Insert gate checks in `dispatch.rs` POST, PATCH, PUT, DELETE handlers
- [x] Return HTTP 423 Locked with structured error JSON
- [x] Update bootstrap to mark system entities as pre-verified
- [x] Update compile_first_e2e tests to mark entities as pre-verified

### Phase 3: Enrich UI Verification Display
- [x] Add `VerificationDetail` type to `observe/lib/types.ts`
- [x] Update `VerificationLevel.details` to accept `VerificationDetail[] | string`
- [x] Add `VerificationDetailsPanel` to `CascadeResults.tsx` for array details
- [x] Updated `LevelDetailPanel` to route array vs string details

### Phase 4: Update Developer Skill
- [x] Update `.claude/skills/temper.md` with active verification polling + 423 note

### Phase 5: Tests
- [x] `operations_blocked_when_verification_pending`
- [x] `operations_blocked_when_verification_running`
- [x] `operations_allowed_after_verification_passes`
- [x] `operations_blocked_after_verification_fails`
- [x] `per_entity_gating_isolation`

## Verification Results
- `cargo test --workspace` — all tests pass (0 failures)
- `cd observe && npx vitest run` — 104 tests pass (13 files)
- 5 new gate tests in multi_tenant.rs all pass
