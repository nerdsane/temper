# ADR-0073: Runtime Index Recovery

- Status: Accepted
- Date: 2026-04-29
- Deciders: Temper core maintainers
- Related:
  - ADR-0058: Query Plane Hot Field Opt-Out and Stable Projections
  - ADR-0060: Bounded Warm Restart and Digest-Aware App Reconcile
  - OpenPaw ADR-0047: Type-Scoped Runtime Index Recovery
  - `crates/temper-server/src/state/projection_backfill.rs`

## Context

Warm restart recovery has two related surfaces:

- Rebuild runtime indexes so existing entities can be found after restart.
- Avoid pre-ready fan-out that hydrates too many actors before the server can respond.

The plan asked for a redo-or-remove decision for pre-ready runtime index recovery. OpenPaw documented type-scoped recovery, but Temper still needs the platform-level decision.

## Decision

Temper keeps bounded runtime index recovery, but it is type-scoped and readiness-aware:

- Pre-ready recovery may rebuild only platform-critical entity types needed to route and boot safely.
- Broad tenant/entity scans run after readiness, through bounded batches.
- Query-plane backfill is idempotent and may lag readiness; OData lazy loads remain the correctness fallback.
- Any recovery that would hydrate actors must use the same admission and dispatch budgets as normal traffic.

## Readiness Gates

- Server boot reaches `/readyz` without requiring a full tenant-wide actor hydration.
- Backfill metrics report counts by tenant and entity type.
- Restart-resilience e2e kills the server mid-session, boots from Postgres, and observes either resumed progress or explicit bounded recovery failure.

## Consequences

### Positive

- Restarts do not create startup storms.
- Query-plane correctness is recoverable from event logs and snapshots.

### Negative

- Some collection queries may be incomplete until post-ready backfill catches up unless they fall back to event-store reads.

### DST Compliance

Recovery is a deterministic replay/backfill process over persisted entity state. Timers and retries stay within IOA actions or bounded platform bootstrap code.

## Rollback Policy

Disable broad post-ready backfill and rely on lazy entity loads while keeping platform-critical pre-ready recovery.
