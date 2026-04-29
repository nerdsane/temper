# ADR-0065: Postgres Platform Store and Canonical Schema

- Status: Accepted
- Date: 2026-04-28
- Deciders: Temper core maintainers
- Related:
  - ADR-0033: Multi-tenant isolation
  - ADR-0058: Query-plane hot field opt-out and stable projections
  - ADR-0063: Object store for blob bytes
  - `crates/temper-store-postgres`
  - `crates/temper-store-postgres/migrations/0001_initial.sql`
  - `crates/temper-server/src/platform_store.rs`

## Context

Temper can append entity events to Postgres, but platform concerns still largely route through Turso-only methods: specs, query projection, trajectory records, decisions, policies, installed apps, WASM metadata, secrets, OTS traces, and evolution tables. That makes a Railway Postgres cutover impossible without retaining Turso in the production path.

The current Postgres schema is also not the canonical platform schema. It is a partial event-store schema with a few platform tables bolted on. That creates a silent degradation mode: callers can select a Postgres event store and still lose query-plane, policy, and trajectory behavior.

## Decision

Postgres is the canonical schema source for platform storage. The versioned SQL migrations in `crates/temper-store-postgres/migrations/` own the runtime schema, and `run_migrations` executes them through `sqlx::migrate!()`. The Postgres crate owns a strict superset of the Turso platform tables using native Postgres types:

- `JSONB` for structured payloads and query projections.
- `BYTEA` only for encrypted secrets and legacy inline WASM bytes.
- `TIMESTAMPTZ` for durable timestamps.
- Composite primary keys and GIN/B-tree indexes matching the read paths.

`PostgresEventStore` implements platform and query-plane methods directly. Server code must call backend-neutral methods or `PlatformStore`, and Postgres must never return success by silently no-oping a supported platform concern.

## Rollout Plan

1. Add versioned `sqlx` migration coverage for all platform tables.
2. Add Postgres implementations for specs, query projection, trajectories, apps, policies, pending decisions, WASM metadata, secrets, blobs, OTS, and evolution support tables.
3. Wire `ServerEventStore` and `PlatformStore` so Postgres and Turso expose the same supported platform surface.
4. Keep Turso code available as a selectable backend, but Railway production will set Postgres env vars only.

## Readiness Gates

- Unit tests prove Postgres schema includes the full platform table set.
- Unit tests prove the versioned migration includes the platform table set and RLS setup.
- Query projection calls against `ServerEventStore::Postgres` execute SQL instead of no-oping.
- Local Postgres boot runs migrations idempotently.
- OpenPaw smoke flow persists trajectory, projection, and spec rows to Postgres.

## Consequences

### Positive

- Railway can run with no Turso production dependency.
- Future backend swaps have an explicit parity target.
- Query and observe paths do not silently degrade when Postgres is selected.

### Negative

- The Postgres crate grows from event-store-only into the platform store crate.
- Migration SQL must remain compatible with existing partial Postgres deployments.

### Risks

- Schema drift between the legacy Rust schema constants and versioned Postgres migrations until the constants are fully retired or generated from SQL.
- Some existing callers still name Turso types. Those are compatibility shims and should be retired once the storage abstraction fully owns shared row types.

### DST Compliance

- Schema and SQL methods are I/O boundaries, outside deterministic simulation.
- Server wiring stays deterministic for simulation by leaving `SimPlatformStore` intact and avoiding wall-clock logic in simulation paths.

## Non-Goals

- This ADR does not perform the production cutover.
- This ADR does not remove Turso source code.
- This ADR does not mandate S3/R2 for blobs; ADR-0063 remains the production object-store policy for large blob bytes.

## Alternatives Considered

1. **Shadow-write Turso and Postgres first** — rejected for this incident response because the user chose a maintenance-window cutover.
2. **Keep Turso for platform tables** — rejected because Railway production must have no Turso dependency after cutover.

## Rollback Policy

Keep `TEMPER_EVENT_STORE=turso` and related Turso env vars valid until the cutover has soaked. If Postgres platform parity regresses before production cutover, select Turso again and repair the Postgres implementation behind the same abstraction.
