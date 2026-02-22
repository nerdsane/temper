# Fix Storage Backend Gaps: Redis Atomics, Tests, Turso Docs, Spec Persistence, Trajectory Generalization

**Created:** 2026-02-21
**Status:** Complete

## Phases

- [x] Phase 1: Redis Atomic Append (Lua Script) — EVALSHA with check-and-set
- [x] Phase 2: Redis EventStore Tests — 5 tests gated on REDIS_URL
- [x] Phase 3: Turso Connection Documentation — doc comment on `connection()`
- [x] Phase 4: Spec Metadata Persistence for Turso — specs table + CRUD + startup recovery
- [x] Phase 5: Trajectory Persistence Generalization — Turso + Redis fallbacks + hydration

## Verification

- `cargo check --workspace` — clean
- `cargo test -p temper-store-turso` — 6 passed (including schema test update)
- `cargo test -p temper-store-redis` — 30 passed (unit tests; Redis integration needs REDIS_URL)
- `cargo test -p temper-server` — 20 passed
- `cargo test --workspace` — 647 passed, 0 failed
