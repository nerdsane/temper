# ADR-0068: Turso Write Gate Retrospective and Removal

- Status: Accepted
- Date: 2026-04-28
- Deciders: Temper core maintainers
- Related:
  - ADR-0048: Dispatch Retry and Error Taxonomy
  - ADR-0065: Postgres Platform Store and Canonical Schema
  - ADR-0067: Trajectory Outbox
  - `crates/temper-store-turso/src/store/write_gate.rs`
  - `crates/temper-store-turso/src/retry.rs`

## Context

A remote Turso write gate was introduced to serialize writes. It used a low-capacity semaphore and admission timeout. During the April 28 incident, low-priority trajectory writes hit the admission timeout, then the retry classifier treated timeout text as transient and retried through the same gate. This amplified delay and contributed to user-visible session failure.

The gate was a band-aid around remote-write tail latency. The platform direction is Postgres colocated with the service plus a bounded trajectory outbox.

## Decision

Turso remains selectable but is no longer the production target. While Turso remains in the workspace:

- Gate admission timeouts are typed and excluded from transient retry classification.
- Production deployment docs must not rely on Turso write-concurrency knobs after cutover.
- Fast-path append bypasses and priority lanes are candidates for removal once Postgres soak completes.

## Rollout Plan

1. Add tests for retry classification of write-gate admission timeouts.
2. Ensure gate timeout errors are typed or otherwise explicitly excluded from transient retry.
3. Move hot-path trajectory writes behind the outbox.
4. After Postgres soak, remove Turso gate/priority fast paths and env knobs.

## Readiness Gates

- Retry classification does not retry gate admission timeout errors.
- No production Railway env var points at Turso after cutover.
- Datadog shows Postgres append p99 within rollback threshold for 48 hours.

## Consequences

### Positive

- The incident failure mode cannot compound via retry re-entry.
- Turso cleanup has a documented sequencing point instead of ad hoc removal.

### Negative

- Turso remote deployments may see slower writes without priority/bypass band-aids after final cleanup.

### Risks

- Removing too early could hurt local or forensic Turso users. Cleanup waits until Postgres soak confirms no production traffic depends on the gate.

### DST Compliance

- Retry classification and Turso write gate behavior are external storage I/O concerns and do not change deterministic simulation semantics.

## Non-Goals

- This ADR does not delete Turso support.
- This ADR does not change entity transition semantics.

## Alternatives Considered

1. **Raise gate capacity only** — rejected because it treats a symptom and keeps audit writes coupled to Turso tail latency.
2. **Shadow-write before moving hot paths** — rejected because the selected cutover strategy is maintenance-window migration.

## Rollback Policy

Before production cutover, select Turso env vars. After cutover, restore the previous gate commit only if a Turso deployment must be supported urgently.
