# ADR-0074: Turso to Postgres ETL Methodology

- Status: Accepted
- Date: 2026-04-29
- Deciders: Temper core maintainers
- Related:
  - ADR-0065: Postgres Platform Store and Canonical Schema
  - `crates/temper-cli/src/migrate_turso_to_postgres.rs`

## Context

The Railway cutover uses a maintenance window. The ETL tool therefore needs to be simple, auditable, and repeatable on production-shaped data before the final window.

## Decision

The migration command is the authoritative ETL path:

```text
temper migrate-turso-to-postgres --tenant <tenant|all> --verify [--dry-run] [--from-snapshot]
```

It streams event journals, snapshots, specs, query projections, blobs, policies, decisions, WASM metadata, trajectories, OTS trajectories, evolution rows, and tenant secrets from Turso into Postgres. It writes a manifest containing row counts and checksums for every migrated table.

Dry-runs must target a disposable Railway Postgres database restored from a production Turso snapshot or export. Neon branching is not required for this plan.

## Readiness Gates

- Production-shaped dry-run completes with zero manifest divergence.
- ETL wall time is below the maintenance-window budget with at least 2x headroom.
- A staging `temper serve` boots against the migrated Postgres target and passes `/readyz`.
- A user-facing smoke test exercises Discord DM routing and the Katagami review-job loop against the migrated target before production cutover.

## Consequences

### Positive

- The cutover has one writer and one verification artifact.
- Production Turso remains unchanged until the final flip.

### Negative

- The approach accepts downtime and does not provide live shadow-write confidence.

### DST Compliance

The ETL is an operational command, not a runtime orchestration path. It must not introduce background business logic.

## Rollback Policy

Before the final flip, discard the target Postgres database. During cutover, set storage env vars back to Turso and restart; Turso remains the source of truth until the verified cutover succeeds.
