# Plan: Live Spec Loading + temper-user Skill Install

**Date**: 2026-02-18
**Status**: COMPLETE

## Summary

Two demo blockers resolved:
1. `/temper-user` skill now installs globally via `temper install`
2. `POST /observe/specs/load-dir` endpoint enables live spec loading into a running server

## Phases

### Phase 1: Install temper-user Globally [COMPLETE]
- Added `USER_SKILL_BODY` and `USER_SKILL_FRONTMATTER` constants
- Updated `install_to()` to create `temper-user/SKILL.md` alongside `temper/SKILL.md`
- Legacy `temper-user.md` bare file cleaned up during install
- 2 new tests: `user_skill_content_is_embedded`, `install_creates_user_skill`

### Phase 2: POST /observe/specs/load-dir [COMPLETE]
- New endpoint reads CSDL + IOA files from a directory
- Registers tenant in shared registry
- Emits SSE events: `spec_loaded`, `verify_started`, `verify_level` (per L0-L3), `verify_done`
- Spawns background verification via `tokio::spawn` + `spawn_blocking`
- Converts `CascadeResult` → `EntityVerificationResult` for registry
- 3 new tests: `test_load_dir_registers_specs`, `test_load_dir_missing_dir_returns_error`, `test_load_dir_emits_design_time_events`

### Phase 3: Temper Skill Workflow Update [COMPLETE]
- Reordered: Start Server → Interview → Generate Specs → Push Specs → Watch Verification → Confirm Ready
- Added `curl POST /observe/specs/load-dir` step after writing specs
- Updated CLI Quick Reference with new workflow

### Phase 4: Tests [COMPLETE]
- `cargo test -p temper-cli -- install`: 7/7 passed
- `cargo test -p temper-server --features observe`: 94/94 passed + 8 multi-tenant + 7 reaction
- `cd observe && npx vitest run`: 104/104 passed

## Files Modified

| File | Change |
|------|--------|
| `crates/temper-cli/src/install/mod.rs` | temper-user skill embedding + install + tests |
| `crates/temper-server/src/observe_routes.rs` | `POST /observe/specs/load-dir` endpoint + tests |
| `.claude/skills/temper.md` | Reordered workflow: server first, push specs via load-dir |
