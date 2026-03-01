# ADR-0017: Platform-Level Deterministic Simulation Testing

- Status: Accepted
- Date: 2026-02-28
- Deciders: Temper core maintainers
- Related:
  - ADR-0016: Verification cascade hardening
  - `.vision/POSITIONING.md` (platform integrity)
  - `crates/temper-store-postgres/` (existing EventStore impl)
  - `crates/temper-server/src/event_store.rs` (ServerEventStore enum)
  - `crates/temper-runtime/src/scheduler/context.rs` (sim context)

## Context

Temper's verification cascade proves agent-submitted specs correct. But the platform code that *executes* those specs — persistence, authorization, hot-swap, multi-tenant routing — is tested only through conventional tests. A concurrency bug in the platform can violate a proven invariant at runtime.

FoundationDB/TigerBeetle principle: **swap the I/O, keep the code.** Don't build a parallel simulation implementation. Run the SAME production code with simulated backends.

Temper already has this pattern — `ServerEventStore` is an enum dispatching across Postgres/Turso/Redis. We extend this to add a `Sim` variant. The `WasmHost` trait already has `SimWasmHost`. Cedar `AuthzEngine` is already pure.

## Decision

### Sub-Decision 1: SimEventStore as a New Crate

Create `temper-store-sim` — an in-memory, deterministic `EventStore` implementation using `BTreeMap` journals. Supports fault injection (write failures, concurrency violations, journal truncation) controlled by a seeded RNG.

**Why this approach**: Same pattern as temper-store-postgres and temper-store-turso. Separate crate keeps simulation deps out of production builds.

### Sub-Decision 2: Sim Variant in ServerEventStore

Add `#[cfg(test)] Sim(SimEventStore)` variant to the `ServerEventStore` enum. Match arms in all `EventStore` methods forward to `SimEventStore`. Production binary is unchanged.

**Why this approach**: The existing enum dispatch pattern avoids dyn-trait overhead and keeps the server's concrete type resolution. `#[cfg(test)]` ensures zero production impact.

### Sub-Decision 3: PlatformDst Test Harness

A test fixture (`PlatformDst`) wires a real `ServerState` with `ServerEventStore::Sim`, `SimWasmHost`, real `AuthzEngine`, and real `SpecRegistry`. This is NOT a separate implementation — it's the real platform with simulated I/O.

**Why this approach**: FoundationDB's `g_network` swap pattern. Same code path, different backend. Bugs found in simulation are bugs in the real code.

### Sub-Decision 4: BUGGIFY Macro

Thread-local fault injection inside production code paths. `#[cfg(not(test))]` compiles to `false` (zero-cost). `#[cfg(test)]` checks a thread-local seeded RNG for probabilistic fault injection.

**Why this approach**: FoundationDB's BUGGIFY pattern. Fault injection at I/O boundaries (SimEventStore) is necessary but insufficient — bugs hide in the code between I/O calls.

### Sub-Decision 5: Multi-Seed Testing

Every DST scenario runs across 100+ seeds. A determinism canary verifies same seed → identical final state. Seed is reported on failure for reproduction: `TEMPER_DST_SEED=42 cargo test dst_replay`.

**Why this approach**: Single-seed tests give false confidence. Multi-seed exposes timing-dependent bugs that single runs miss.

## Rollout Plan

1. **Phase 1 (Immediate)** — SimEventStore crate + Sim variant + persistence DST tests.
2. **Phase 2 (Follow-up)** — SimWasmHost already exists; add DST integration tests.
3. **Phase 3** — Full PlatformDst harness: hot-swap, multi-tenant, lifecycle, cross-entity.
4. **Phase 4** — BUGGIFY macro + injection points in production code.
5. **Phase 5** — Workload generator + CI integration (nightly 1000-seed runs).

## Readiness Gates

- All 430+ existing tests still pass after each phase.
- New DST tests pass across 100+ seeds with heavy fault injection.
- Determinism canary: same seed → identical final state.
- No new HashMap/Instant::now/thread::spawn in simulation-visible crates.

## Consequences

### Positive
- Platform bugs caught before production, with seed-reproducible failures.
- Same code tested in simulation and production — no simulation-only bugs.
- Foundation for future adversarial schedule exploration (turmoil integration).

### Negative
- SimEventStore must be kept in sync with EventStore trait changes.
- BUGGIFY points add minimal cognitive overhead to production code.

### Risks
- SimEventStore may not perfectly model Postgres failure modes. Mitigation: fault injection covers the common cases; edge cases can be added incrementally.
- Single-threaded tokio doesn't explore all interleavings. Mitigation: documented as future work (VOPR-style exploration).

### DST Compliance
- `temper-store-sim` uses only `BTreeMap` (deterministic iteration order).
- Fault injection seeded by `DeterministicRng` — same seed, same faults.
- Tests install `SimContext` via `install_deterministic_context(seed)`.
- No `// determinism-ok` annotations needed — everything is deterministic by construction.

## Non-Goals

- Adversarial schedule exploration (turmoil) — future work.
- Distributed simulation (multi-node) — single-node only per POSITIONING.md.
- Storage corruption testing (TigerBeetle 8-9% corruption) — future phase.

## Alternatives Considered

1. **Separate SimPlatformDispatcher** — Reimplements platform logic for simulation. Rejected: diverges from production code, bugs in simulation != bugs in production.
2. **Mocking with mockall** — Dynamic mocks for EventStore. Rejected: doesn't test the real dispatch path, mock setup is brittle and doesn't compose.
3. **Docker-based integration tests** — Run real Postgres in CI. Rejected: slow, non-deterministic, can't reproduce failures by seed.

## Rollback Policy

SimEventStore is `#[cfg(test)]` only. Removing it has zero production impact. Delete the crate and remove the Sim variant from the enum.
