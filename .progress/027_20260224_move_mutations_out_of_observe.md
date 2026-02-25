# 027 — Move Mutation Endpoints Out of /observe

## Goal
Move all 7 POST/DELETE mutation endpoints from `/observe` (read-only) into `/api/*`.

## Endpoints to Move

| # | Old Path | New Path | Method |
|---|----------|----------|--------|
| 1 | `/observe/specs/load-dir` | `/api/specs/load-dir` | POST |
| 2 | `/observe/specs/load-inline` | `/api/specs/load-inline` | POST |
| 3 | `/observe/wasm/modules/{name}` | `/api/wasm/modules/{name}` | POST |
| 4 | `/observe/wasm/modules/{name}` | `/api/wasm/modules/{name}` | DELETE |
| 5 | `/observe/evolution/records/{id}/decide` | `/api/evolution/records/{id}/decide` | POST |
| 6 | `/observe/trajectories/unmet` | `/api/evolution/trajectories/unmet` | POST |
| 7 | `/observe/sentinel/check` | `/api/evolution/sentinel/check` | POST |

## Phases

### Phase 1: Backend — Create `/api` router
- [x] Make observe sub-modules `pub(crate)` in `observe/mod.rs`
- [x] Create `crates/temper-server/src/api.rs` with `build_api_router()`
- [x] Remove mutation routes from `build_observe_router()`
- [x] Wire `/api` into `router.rs`

### Phase 2: Update backend tests
- [x] Update observe/mod.rs tests referencing moved endpoints (9 test URLs updated)

### Phase 3: Frontend updates
- [x] Update `observe/lib/api.ts` — `triggerSentinelCheck` path
- [x] Update `observe/__tests__/lib/api.test.ts` — sentinel check URL
- [x] Update `observe/app/integrations/page.tsx` — help text

### Phase 4: Docs + Skills
- [x] Update `.claude/skills/temper.md`
- [x] Update `.claude/skills/temper-user.md`
- [x] Update `.claude/commands/temper-user.md`
- [x] Update `observe/public/skills/builder.md`
- [x] Update `observe/public/skills/user.md`
- [x] Update `docs/GAP_TRACKER.md`
- [x] Update `skills/temper/SKILL.md`

### Phase 5: Verify
- [x] `cargo test -p temper-server` — 145 tests pass
- [x] `cd observe && npx vitest run` — 104 tests pass
- [x] Reviews (DST, code, alignment) — all PASS
- [x] Readability baseline updated (PROD_MAX_FILE_LINES 1229→1234 from upstream specs_helpers extraction)
- [x] Committed: 51f0e6d
- [x] Pushed to main

## Status: COMPLETE
