# ADR-0066: StorageStack Backend Selection

- Status: Accepted
- Date: 2026-04-28
- Deciders: Temper core maintainers
- Related:
  - ADR-0065: Postgres Platform Store and Canonical Schema
  - `crates/temper-server/src/event_store.rs`
  - `crates/temper-cli/src/serve/bootstrap.rs`

## Context

`ServerEventStore` is a concrete enum. Platform helper methods match on that enum and historically returned `Ok(())` or `Ok(None)` for non-Turso variants. That was acceptable while Turso was the only full platform backend, but it becomes dangerous once Postgres is selected for production: unsupported branches can look healthy while dropping query-plane or platform writes.

## Decision

Temper will treat storage as a composed stack with explicit backend labels and feature support:

- Event journal and snapshots.
- Platform metadata.
- Durable query projection.
- Trajectory sink.

The initial rollout keeps `ServerEventStore` as the concrete event-store carrier because `EventStore` uses `impl Future` methods and is not object-safe. However, `ServerEventStore` methods become delegation-only for supported concerns: Postgres branches call Postgres SQL, Turso branches call Turso SQL, and unsupported backends return explicit lack-of-support only when the caller can safely degrade.

Environment-driven bootstrap selects backend with:

- `TEMPER_EVENT_STORE=postgres|turso`
- `TEMPER_PLATFORM_STORE=postgres|turso`
- `TEMPER_QUERY_PROJECTION_STORE=postgres|turso|disabled`

Unset values preserve current behavior until cutover.

## Rollout Plan

1. Remove no-op Postgres branches for query projection.
2. Make `platform_store()` return Postgres once Postgres implements `PlatformStore`.
3. Add tests that fail if Postgres platform branches no-op.
4. Introduce a full `StorageStack` newtype once the runtime `EventStore` trait has an object-safe adapter.

## Readiness Gates

- Postgres query projection round-trips in tests.
- Startup logs expose the selected backend label.
- Redis remains explicit ephemeral mode and returns clear unsupported errors for platform metadata.

## Consequences

### Positive

- Backend selection becomes observable and testable.
- Postgres can be promoted without hidden Turso-only assumptions.

### Negative

- The concrete enum remains until the runtime trait is adapted.

### Risks

- Mixed backend configuration can create split-brain platform data. Cutover documentation must set all production stores to Postgres together.

### DST Compliance

- Env var reads happen at startup only and are annotated as external configuration. Simulation remains on `SimEventStore`/`SimPlatformStore`.

## Non-Goals

- This ADR does not remove Redis ephemeral mode.
- This ADR does not add per-tenant Postgres schema routing; row-level tenant columns remain the first implementation.

## Alternatives Considered

1. **Immediate trait-object rewrite** — rejected as too broad while the runtime trait is not object-safe.
2. **Continue enum matches with no-ops** — rejected because this caused silent backend degradation.

## Rollback Policy

Set storage env vars back to Turso before cutover. The code keeps Turso selectable while Postgres parity is verified.
