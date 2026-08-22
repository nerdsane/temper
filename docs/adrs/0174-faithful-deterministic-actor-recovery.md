# ADR-0174: Faithful deterministic actor recovery and fault delivery

- Status: Accepted
- Date: 2026-08-21
- Deciders: Temper core maintainers
- Related:
  - ADR-0032: Platform store trait and simulated platform DST
  - ADR-0046: Durable cross-entity reactions
  - `crates/temper-runtime/src/scheduler`
  - `crates/temper-server/src/entity_actor`

## Context

Temper's deterministic actor simulation shares transition evaluation and effect application with production, but four remaining seams weaken the proof. Scheduler restart currently changes only the scheduler's actor-state flag and retains the same mutable handler. Spec invariant evaluation treats unknown counters as successful and infers terminality from the previous state instead of enabled actions. Simulated callbacks execute directly instead of traversing the fault-injecting scheduler. Event-history bookkeeping is duplicated, and the ordinary simulation action path does not advance the durable sequence like production.

The test corpus also lacks deterministic clock anomalies and auditable, exhaustive coverage manifests for the in-scope platform and reference-app specs. These gaps prevent the DST setup from demonstrating FoundationDB-style recovery and delivery fidelity even though its core transition architecture is shared.

## Decision

### Sub-Decision 1: Reconstruct handlers from their deterministic journal

`SimActorHandler` will expose a restart operation. `EntityActorHandler` will implement it by copying the bounded recorded journal, creating fresh initial `EntityState`, and replaying each event through the production replay semantics. `SimScheduler` will report actor restart edges, and `SimActorSystem` will invoke reconstruction exactly when a crashed actor becomes running.

**Why this approach**: It tests loss and recovery of volatile state without introducing a second actor implementation or retaining the mutated state under a new scheduler flag.

### Sub-Decision 2: Make invariant inputs explicit and total

The simulation-handler contract will expose named counter values and enabled actions. Counter assertions will fail when their declared variable is unavailable, and `no_further_transitions` will inspect the actual enabled-action set. Unsupported invariant expressions will be surfaced during handler construction rather than silently omitted.

**Why this approach**: A passing invariant must be evidence about concrete state. Missing evidence cannot count as success.

### Sub-Decision 3: Deliver actor messages through one faulting queue

`SimActorSystem` will provide bounded queued actor-to-actor dispatch. Cross-entity test workloads will use that path so delay, drop, crash, and restart faults apply to the real target handlers. Direct scripted `step` remains available for precise unit scenarios.

**Why this approach**: The scheduler is already the deterministic network abstraction. Extending its use is smaller and more faithful than creating a separate trigger simulator.

### Sub-Decision 4: Model clock anomalies explicitly

The logical clock will support deterministic forward jumps and signed actor-local skew without allowing the global tick to move backward. Simulation records will include the effective timestamps produced by those anomalies.

**Why this approach**: Forward jumps cover timeout discontinuities; local skew covers inconsistent actor observations while preserving a monotonic scheduler.

### Sub-Decision 5: Share committed-event bookkeeping

Live handling, replay, field updates, and simulation will use one helper to apply the committed sequence number and append an event to bounded history. Persistence still decides the authoritative sequence in production; simulation deterministically uses the next sequence.

**Why this approach**: Transition effects are already unified. Unifying the final commit step closes the remaining state-history parity seam without simulating production telemetry or storage internals.

### Sub-Decision 6: Coverage is generated and auditable

Coverage tests will parse every in-scope platform, ecommerce, and on-call spec, derive declared entities/states/actions, and compare them with deterministic generated execution coverage. Full-lane seed budgets will provide at least 1,000 random/property scenarios, with smoke budgets kept bounded for ordinary workspace feedback.

**Why this approach**: A generated manifest fails when a spec grows without DST coverage, avoiding manually maintained counts that silently become stale.

## Rollout Plan

1. Add the shared runtime contracts, event bookkeeping, restart reconstruction, and clock controls.
2. Add faulted cross-entity and exhaustive generated coverage tests.
3. Resolve all determinism audit findings and run the full validation/review/deployment gates.

## Readiness Gates

- No invariant evaluator succeeds because its input is absent.
- Same-seed runs, including restarts and clock anomalies, produce identical records.
- Cross-entity heavy-fault runs exercise delivery, drop, crash, and reconstructed restart.
- In-scope action/state coverage manifests are complete and at least 70% of scenarios are generated.
- The determinism audit has zero unsuppressed findings.

## Consequences

### Positive

- Actor crash/restart tests now prove recovery rather than flag toggling.
- Invariant results become trustworthy and coverage drift becomes a test failure.
- Cross-entity and time-dependent behavior uses deterministic fault controls.
- Live and simulated event history cannot diverge through separate bookkeeping.

### Negative

- `SimActorHandler` gains additional recovery and introspection responsibilities.
- Exhaustive full-lane coverage costs more CPU, so smoke and full seed budgets remain distinct.

### Risks

- Replay bugs could be masked if reconstruction copies derived state. The implementation may retain only journal inputs and must rebuild all derived fields.
- Clock skew could leak between actors. Skew is scoped to an explicit actor dispatch and reset after delivery.

### DST Compliance

- All new ordering-sensitive collections use `BTreeMap`/`BTreeSet`.
- Clock anomalies and restart choices remain seed-controlled and replayable.
- `// determinism-ok` is reserved for production-only elapsed-time metrics or external-I/O deadlines that do not enter simulated state; each annotation states that boundary.

## Non-Goals

- Simulating Postgres, Turso, HTTP, or OTEL internals inside `SimActorSystem`.
- Weakening existing verification, persistence, security, or backend-parity gates.
- Changing application entity behavior or hardcoding entity-specific states in framework code.

## Alternatives Considered

1. **Keep restart as a scheduler-only flag** — rejected because mutable handler state survives the modeled crash.
2. **Return success for unavailable invariant inputs** — rejected because it creates vacuous proofs.
3. **Call target handlers directly for cross-entity tests** — rejected because it bypasses delay, drop, crash, and restart injection.
4. **Use wall-clock sleeps for time anomalies** — rejected because they are slow and non-reproducible.

## Rollback Policy

The simulation APIs and tests can be reverted independently of production storage formats. The shared committed-event helper may be inlined back into callers if necessary; no persisted schema or event payload migration is introduced.

## Validation

- Generated exploration loads all 28 maintained platform, ecommerce, and on-call specs and requires exact equality with every declared state, non-output action, and invariant evaluator.
- The focused recovery suite covers journal reconstruction, heavy-fault actor messaging across 64 seeds, actor-local skew, forward time jumps, and non-vacuous counter and terminal invariants.
- The counted DST suites contain 31 generated/random tests and 3 scripted aggregation tests (91% generated/random).
- `scripts/check-determinism.sh` reports zero unsuppressed findings after each production-only exception was narrowly documented.
