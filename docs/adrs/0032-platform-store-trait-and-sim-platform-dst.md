# ADR-0032: PlatformStore Trait and Simulation-Level Platform DST

- Status: Proposed
- Date: 2026-03-16
- Deciders: Temper core maintainers
- Related:
  - ADR-0017: Platform-level deterministic simulation testing (entity actor DST)
  - ADR-0030: Hash-gated verification
  - `crates/temper-server/src/event_store.rs` (ServerEventStore enum)
  - `crates/temper-store-turso/` (TursoEventStore, platform persistence)
  - `crates/temper-store-sim/` (SimEventStore, entity-level simulation)
  - `crates/temper-server/src/platform/` (bootstrap, install_os_app, recover_cedar_policies)

## Context

ADR-0017 introduced deterministic simulation testing for entity actors — the `SimEventStore` crate, `ServerEventStore::Sim` variant, and `PlatformDst` test harness. That work proved the value of running real production code against simulated I/O: entity lifecycle, state transitions, and event sourcing are now tested under fault injection across hundreds of seeds.

However, ADR-0017's scope stopped at the entity actor layer. **Platform-level operations — the code that bootstraps tenants, installs OS apps, persists Cedar policies, manages spec storage, and maintains entity indexes — remain untested under simulation.** This is because platform persistence flows through a completely separate path:

```
// Current pattern in platform code:
if let Some(turso) = store.platform_turso_store() {
    turso.save_spec(...).await?;
}
// When ServerEventStore::Sim is used, platform_turso_store() returns None.
// ALL platform persistence is silently skipped.
```

The result: simulation tests exercise entity actors in a vacuum. The platform bootstrap code, spec persistence, Cedar policy recovery, OS app installation, and entity index management literally do not run under simulation. Production code paths that are critical to correctness are invisible to DST.

**This gap has caused recurring bugs:**

- **OS app specs lost on restart**: `install_os_app` persisted specs to Turso, but recovery on boot failed to reload them. Only caught in manual testing after deploy.
- **Cedar policies not enforcing after boot**: `recover_cedar_policies` loaded policies from Turso, but a missing await caused the authorization engine to start empty. Simulation would have caught the boot sequence.
- **Entity fields lost on replay**: Field definitions stored in platform tables were not replayed during spec recovery, causing entity actors to start with stale schemas.
- **Deleted entities resurrecting**: Tombstones were written to the entity event journal but not to the platform index. After restart, the index listed the entity as alive.
- **Entity-set-not-found after redeploy**: Spec redeployment updated the spec registry but failed to update the platform store index, causing OData queries to return 404.

Every one of these bugs lived in the gap between entity actor simulation and platform persistence. A `PlatformStore` trait with a simulation implementation would have caught them all.

## Decision

### Sub-Decision 1: PlatformStore Trait

Extract the ~15 platform storage methods currently implemented directly on `TursoEventStore` into an async trait `PlatformStore`. This trait abstracts all platform-level persistence behind a single interface:

```rust
#[async_trait]
pub trait PlatformStore: Send + Sync {
    // Spec management
    async fn save_spec(&self, tenant: &TenantId, entity_type: &str, spec: &IoaSpec) -> Result<()>;
    async fn load_spec(&self, tenant: &TenantId, entity_type: &str) -> Result<Option<IoaSpec>>;
    async fn list_specs(&self, tenant: &TenantId) -> Result<Vec<String>>;
    async fn delete_spec(&self, tenant: &TenantId, entity_type: &str) -> Result<()>;

    // Cedar policy management
    async fn save_cedar_policies(&self, tenant: &TenantId, policies: &CedarPolicies) -> Result<()>;
    async fn load_cedar_policies(&self, tenant: &TenantId) -> Result<Option<CedarPolicies>>;

    // OS app management
    async fn save_installed_app(&self, tenant: &TenantId, app_id: &str, app_meta: &AppMetadata) -> Result<()>;
    async fn load_installed_apps(&self, tenant: &TenantId) -> Result<Vec<(String, AppMetadata)>>;
    async fn delete_installed_app(&self, tenant: &TenantId, app_id: &str) -> Result<()>;

    // Entity index management
    async fn save_entity_index(&self, tenant: &TenantId, entity_type: &str, entity_id: &str, metadata: &EntityIndexEntry) -> Result<()>;
    async fn load_entity_index(&self, tenant: &TenantId, entity_type: &str) -> Result<Vec<EntityIndexEntry>>;
    async fn delete_entity_index(&self, tenant: &TenantId, entity_type: &str, entity_id: &str) -> Result<()>;

    // Decision/evolution records
    async fn save_decision(&self, tenant: &TenantId, decision: &DecisionRecord) -> Result<()>;
    async fn load_decisions(&self, tenant: &TenantId) -> Result<Vec<DecisionRecord>>;

    // WASM module storage
    async fn save_wasm_module(&self, tenant: &TenantId, module_id: &str, bytes: &[u8]) -> Result<()>;
    async fn load_wasm_module(&self, tenant: &TenantId, module_id: &str) -> Result<Option<Vec<u8>>>;
}
```

**Why this approach**: The current `platform_turso_store() -> Option<&TursoEventStore>` pattern couples all platform code to the concrete Turso type. This makes simulation impossible — there is no trait to implement for a sim backend. Extracting a trait follows the same pattern ADR-0017 used for entity event sourcing: abstract the I/O, keep the code.

### Sub-Decision 2: SimPlatformStore

Create `SimPlatformStore` — an in-memory, deterministic implementation of `PlatformStore` using `BTreeMap`-based storage. Each storage domain (specs, policies, apps, indexes, decisions, WASM) is a separate `BTreeMap` behind a `std::sync::Mutex`.

Per-operation fault injection follows the BUGGIFY pattern from ADR-0017: a seeded `DeterministicRng` probabilistically injects failures at each method call. Fault configuration uses `SimPlatformFaultConfig` with per-operation probabilities:

- **Write failure** (`spec_write_failure_prob`, `policy_write_failure_prob`, `app_write_failure_prob`): `save_*` returns `Err` without persisting (simulates disk/network failure)
- **Read failure** (`spec_read_failure_prob`, `decision_read_failure_prob`): `load_*` returns `Err` (simulates I/O failure on read)
- **Cleanup failure** (`cleanup_failure_prob`): `delete_spec` returns `Err` without removing (simulates failed cleanup during partial-write rollback — discovered a real orphan bug, see Consequences)
- **Decision write failure** (`decision_write_failure_prob`): `save_pending_decision` returns `Err`

`SimPlatformFaultConfig::none()` sets all probabilities to 0.0; `SimPlatformFaultConfig::heavy()` uses elevated rates (0.02–0.05) for stress testing.

```rust
pub struct SimPlatformStore {
    specs: Mutex<BTreeMap<(TenantId, String), IoaSpec>>,
    policies: Mutex<BTreeMap<TenantId, CedarPolicies>>,
    apps: Mutex<BTreeMap<(TenantId, String), AppMetadata>>,
    indexes: Mutex<BTreeMap<(TenantId, String, String), EntityIndexEntry>>,
    decisions: Mutex<BTreeMap<TenantId, Vec<DecisionRecord>>>,
    wasm: Mutex<BTreeMap<(TenantId, String), Vec<u8>>>,
    fault_rng: Mutex<DeterministicRng>,
    fault_rate: f64,
}
```

**Why this approach**: In-memory `BTreeMap` storage is deterministic (iteration order is sorted), zero-dependency (no Turso/SQLite), and fast (thousands of seeds per second). `Mutex` rather than `RwLock` because DST runs single-threaded — contention is impossible but `Mutex` is simpler. Per-domain maps mirror the Turso table structure, so the simulation exercises the same logical schema.

### Sub-Decision 3: Wire into ServerEventStore

Replace `platform_turso_store() -> Option<&TursoEventStore>` with `platform_store() -> Option<&dyn PlatformStore>`. Both `TursoEventStore` and `SimPlatformStore` implement the trait:

```rust
impl ServerEventStore {
    pub fn platform_store(&self) -> Option<&dyn PlatformStore> {
        match self {
            Self::Turso(store) => Some(store as &dyn PlatformStore),
            #[cfg(test)]
            Self::Sim(store) => Some(store.platform_store()),
            _ => None,
        }
    }
}
```

The `SimEventStore` from ADR-0017 gains a `SimPlatformStore` field, so entity-level and platform-level simulation share the same `ServerEventStore::Sim` variant.

**Why this approach**: Minimal change to the existing dispatch pattern. `platform_store()` replaces `platform_turso_store()` at all call sites — a mechanical refactor with no behavioral change for production. The Sim variant now returns a real implementation instead of `None`.

### Sub-Decision 4: Update All Call Sites

Every platform code path that currently calls `platform_turso_store()` is updated to call `platform_store()`. The pattern changes from:

```rust
// Before: silently skips in simulation
if let Some(turso) = store.platform_turso_store() {
    turso.save_spec(tenant, entity_type, &spec).await?;
}

// After: runs in both production and simulation
if let Some(ps) = store.platform_store() {
    ps.save_spec(tenant, entity_type, &spec).await?;
}
```

Call sites include: `install_os_app`, `bootstrap_tenants`, `recover_cedar_policies`, `deploy_spec`, `undeploy_spec`, `create_entity`, `delete_entity`, `update_entity_index`, `load_entity_set`, and approximately 5 others.

**Why this approach**: The refactor is mechanical — same control flow, same error handling, different method name. No behavioral change for production. But in simulation, these paths now execute against `SimPlatformStore` instead of being skipped.

### Sub-Decision 5: Platform Invariants (P1–P17)

Seventeen formal invariants are checked during simulated operations. These are the properties that define correct platform behavior:

| ID  | Name                          | Property                                                                                          |
|-----|-------------------------------|---------------------------------------------------------------------------------------------------|
| P1  | Registry-Store Consistency    | Every spec in `SpecRegistry` has a corresponding entry in `PlatformStore::load_spec`              |
| P2  | Store-Registry Consistency    | Every spec in `PlatformStore::list_specs` is loaded in `SpecRegistry` (no orphan specs)           |
| P3  | Index-Store Agreement         | Every entity in the index has a valid event journal in `EventStore`                               |
| P4  | Store-Index Completeness      | Every entity with events in `EventStore` appears in the platform index                            |
| P5  | Tombstone Finality            | A deleted entity (tombstoned in index) never reappears in `load_entity_index`                     |
| P6  | Cedar-Spec Coherence          | For every deployed spec, the Cedar engine contains matching action policies                       |
| P7  | Cedar Persistence             | `load_cedar_policies` returns policies consistent with the current `AuthzEngine` state            |
| P8  | State-Store Sequence Agreement| Entity state in memory matches the state derived from replaying its event journal                 |
| P9  | Rollback Completeness         | After `undeploy_spec`, the spec, index entries, and Cedar policies are all removed                |
| P10 | Field Replay Fidelity         | Entity field values survive a full stop-replay cycle (save events → clear memory → replay)        |
| P11 | Installed Apps Persistence    | `load_installed_apps` returns all apps installed via `save_installed_app` that were not deleted    |
| P12 | Bootstrap Idempotence         | Running `bootstrap_tenants` twice produces the same state as running it once                      |
| P13 | Sequence Monotonicity         | Event sequence numbers in each entity journal are strictly increasing with no gaps                |
| P14 | Tenant Isolation              | Operations on tenant A never affect specs, policies, indexes, or events for tenant B              |
| P15 | Initial State Correctness     | A newly created entity's state matches the spec's `initial_state`                                 |
| P16 | Event Replay Through TransitionTable | For each indexed entity, the stored event sequence represents valid transitions in the TransitionTable (structural check: rule exists with matching action + from/to states, without re-evaluating guards since `EvalContext` isn't stored in events) |
| P17 | Spec Roundtrip Equivalence    | For each registered spec, rebuilding a `TransitionTable` from stored IOA source produces an equivalent table (matching `initial_state`, sorted `states`, `rules.len()`, and per-rule `name`, `from_states`, `to_state`) |

**Why this approach**: Each invariant corresponds to a real bug class observed in production or testing. P5 catches the resurrecting-entity bug. P11 catches the OS-app-lost-on-restart bug. P12 catches the bootstrap-idempotence bug. P10 catches the field-replay bug. P16 catches event-journal corruption or TransitionTable drift. P17 catches spec serialization/deserialization divergence. Formal invariants checked under fault injection are far more powerful than point-in-time assertions.

**Invariant tiering for mid-operation checks**: P1/P2 can be transiently violated when `install_os_app` fails mid-write AND cleanup `delete_spec` also fails due to fault injection. These orphans are reconciled on the next clean restart by `restore_registry_from_platform_store`. Mid-workload, only invariants immune to transient orphans are checked (P8, P9, P13 via `assert_mid_operation_invariants`). Full P1/P2 are validated after the final clean restart (faults disabled) in each test.

### Sub-Decision 6: DST Test Suites

Four test suites exercise platform operations under fault injection, each running across 100+ seeds:

**Suite 1: Boot Cycle** (`dst_platform_boot_cycle`)
- Install OS apps → deploy specs → create entities → simulate restart (clear in-memory state) → bootstrap → verify P1, P2, P3, P4, P8, P10, P11, P12, P15
- Fault injection: write failures during install, partial saves during deploy

**Suite 2: Rollback** (`dst_platform_rollback`)
- Deploy spec → create entities → undeploy spec → verify P5, P9
- Deploy new version of spec → verify entities migrate correctly → verify P1, P2, P8
- Fault injection: failures during undeploy, partial cleanup

**Suite 3: Cedar Lifecycle** (`dst_platform_cedar_lifecycle`)
- Deploy spec with Cedar policies → verify P6, P7
- Update spec (new actions) → verify policies update → verify P6, P7
- Undeploy → verify policies removed → verify P9
- Fault injection: failures during policy save, stale reads during recovery

**Suite 4: Index Consistency** (`dst_platform_index_consistency`)
- Create entities across multiple types → verify P3, P4
- Delete entities → verify P5
- Concurrent creates/deletes across tenants → verify P13, P14
- Fault injection: partial deletes, stale index reads

**Why this approach**: Each suite targets a specific failure class. The boot cycle suite alone would have caught 4 of the 5 recurring bugs listed in the Context section. Multi-seed runs with fault injection explore the space of possible failure orderings far better than manually written test cases.

## Rollout Plan

1. **Phase 0 (Immediate)** — `PlatformStore` trait definition, `SimPlatformStore` implementation, wire into `ServerEventStore::Sim`, update call sites from `platform_turso_store()` to `platform_store()`. Boot cycle test suite (Suite 1) with invariants P1–P4, P8, P10–P12, P15.
2. **Phase 1 (Follow-up)** — Cedar lifecycle suite (Suite 3) with P6, P7. Rollback suite (Suite 2) with P5, P9. Index consistency suite (Suite 4) with P3–P5, P13, P14. All 17 invariants active.
3. **Phase 2 (CI Integration)** — Combined chaos testing: all four suites run with higher fault rates. Nightly CI job runs 1000-seed sweeps. Determinism canary: same seed produces identical final state.

## Readiness Gates

- All existing 430+ tests pass after trait extraction (Phase 0 gate).
- Boot cycle suite passes across 100 seeds with 10% fault rate.
- All 17 invariants pass across 100 seeds in all four suites (Phase 1 gate).
- Determinism canary: two runs with the same seed produce byte-identical `SimPlatformStore` state.
- No new `HashMap`, `Instant::now`, or `thread::spawn` in simulation-visible code.
- `TursoEventStore` implements `PlatformStore` with no behavioral change (verified by existing integration tests).

## Consequences

### Positive
- Catches an estimated ~80% of past platform bugs (OS app persistence, Cedar recovery, index consistency, field replay, bootstrap idempotence) automatically under simulation.
- Platform bootstrap code — the most critical and least tested path — now runs under DST with fault injection.
- Formal invariants (P1–P17) document the platform's correctness contract explicitly, serving as both tests and specification.
- Same production code runs in simulation — bugs found are real bugs, not simulation artifacts.
- Seed-reproducible failures: every platform bug can be replayed with `TEMPER_DST_SEED=N`.

### Negative
- Trait extraction touches production code: `platform_turso_store()` → `platform_store()` across ~15 call sites. Mechanical but non-zero risk.
- `TursoEventStore` must implement the new trait, adding a thin delegation layer.
- `SimPlatformStore` must be kept in sync with `PlatformStore` trait changes (same maintenance burden as `SimEventStore` from ADR-0017).

### Risks
- **Trait boundary may not cover all Turso methods**: Some platform operations may use Turso-specific APIs not captured in the trait. Mitigation: audit all `platform_turso_store()` call sites during Phase 0; add missing methods to the trait.
- **Simulation fidelity**: `SimPlatformStore` uses `BTreeMap` while Turso uses SQLite; failure modes differ. Mitigation: fault injection covers the common failure classes (write failure, stale read, partial operation); Turso-specific edge cases are documented as future work.
- **Performance of invariant checking**: Checking 17 invariants after every operation may slow test execution. Mitigation: invariants are cheap (in-memory map lookups); if slow, check only after test phases rather than every operation.

### DST Compliance
- `SimPlatformStore` uses only `BTreeMap` for all storage (deterministic iteration order).
- Fault injection is seeded by `DeterministicRng` — same seed produces identical fault sequence.
- All async operations use `sim_now()` for timestamps and `sim_uuid()` for generated IDs.
- `Mutex` (not `RwLock`) for interior mutability — simpler, deterministic under single-threaded execution.
- No `// determinism-ok` annotations needed — all simulation code is deterministic by construction.
- Tests install `SimContext` via `install_deterministic_context(seed)` before execution.

## Non-Goals

- **Turso-specific failure testing** (WAL corruption, journal mode edge cases) — out of scope; `SimPlatformStore` models logical failures, not storage-engine-specific ones.
- **Distributed simulation** (multi-node platform recovery) — single-node only per POSITIONING.md.
- **Adversarial schedule exploration** (turmoil integration) — future work per ADR-0017.
- **Migration testing** (schema changes between versions) — separate concern; track in a future ADR.
- **Performance benchmarking under simulation** — DST tests verify correctness, not throughput.

## Alternatives Considered

1. **Extend SimEventStore with platform methods directly** — Add platform storage methods to `SimEventStore` without a trait. Rejected: couples entity event sourcing and platform storage in a single struct, violates single-responsibility, and makes it impossible to test platform persistence independently of entity events.
2. **Mock-based platform testing (mockall)** — Create mock implementations of each platform storage function. Rejected: mocks test call sequences, not correctness properties. They cannot express invariants like P5 (tombstone finality) or P12 (bootstrap idempotence). They also diverge from production code paths.
3. **Integration tests against real Turso** — Run platform tests against an in-process SQLite/Turso instance. Rejected: non-deterministic (file I/O timing), cannot inject faults at specific points, cannot reproduce failures by seed. Useful as a complement but not a replacement for DST.
4. **Keep platform_turso_store() and add platform_sim_store()** — Add a parallel accessor for simulation without a unifying trait. Rejected: every call site would need `if sim { ... } else if turso { ... }` branching, which is exactly the problem traits solve.

## Rollback Policy

`PlatformStore` trait and `SimPlatformStore` are `#[cfg(test)]`-gated in their simulation usage. The trait itself is production-visible (replacing the concrete `TursoEventStore` accessor), but rolling back requires only reverting `platform_store()` back to `platform_turso_store()` at each call site — a mechanical change. `SimPlatformStore` can be deleted with zero production impact.
