# ADR-0066: StorageStack Backend Selection

- Status: Accepted
- Date: 2026-04-28
- Deciders: Temper core maintainers
- Related:
  - ADR-0065: Postgres Platform Store and Canonical Schema
  - ADR-0076: Eliminate ServerEventStore Enum Dispatch
  - `crates/temper-server/src/storage/mod.rs`
  - `crates/temper-cli/src/serve/bootstrap.rs`

## Context

Historically, `ServerEventStore` was a concrete enum. Platform helper methods
matched on that enum and returned `Ok(())` or `Ok(None)` for some unsupported
variants. That was acceptable while Turso was the only full platform backend,
but it became dangerous once Postgres was selected for production: unsupported
branches could look healthy while dropping query-plane or platform writes.

## Decision

Temper will treat storage as a composed stack with explicit backend labels and feature support:

- Event journal and snapshots.
- Platform metadata and policy metadata.
- Durable query projection.
- Trajectory sink and observe reads.
- Metadata, blob, authz, evolution, OTS, and WASM side stores.

`crates/temper-server/src/storage/mod.rs` owns the server-facing stack:

- `DynEventStore`, an object-safe adapter over `temper_runtime::persistence::EventStore`.
- `BoxedEventStore`, the cloneable journal/snapshot handle stored in the stack.
- `QueryPlaneStore`, the durable projection capability for projection writes, batched projection reads, OData filter push-down, and projection metrics.
- `TrajectorySink`, the durable observe trajectory write capability.
- `StorageStack`, a composed value containing event, platform, policy,
  query-plane, trajectory, metadata, Turso-provider, Postgres pool, and
  backend-label handles.

`ServerState::set_storage_stack` attaches the selected stack. Query-plane,
trajectory, platform, metadata, observe, evolution, blob, authz, and WASM
callers use stack capabilities instead of matching over backend enums. The old
`ServerEventStore` enum has been removed by ADR-0076.

Environment-driven bootstrap selects backend with:

- `TEMPER_EVENT_STORE=postgres|turso`
- `TEMPER_PLATFORM_STORE=postgres|turso`
- `TEMPER_QUERY_PROJECTION_STORE=postgres|turso|disabled`

Unset values preserve current behavior until cutover.

## Rollout Plan

1. Remove no-op Postgres branches for query projection.
2. Make `platform_store()` return Postgres once Postgres implements `PlatformStore`.
3. Add tests that fail if Postgres platform branches no-op.
4. Introduce `StorageStack` and `DynEventStore` so backend selection produces composed capabilities.
5. Move production query-plane and trajectory callers onto dedicated stack traits.
6. Move long-tail platform/observe/evolution/blob/authz/WASM callers onto
   capability traits and delete the enum dispatch layer.

## Readiness Gates

- Postgres query projection round-trips in tests.
- Startup logs expose the selected backend label.
- Redis remains explicit ephemeral mode and returns clear unsupported errors for platform metadata.
- `cargo test -p temper-server --test storage_stack` proves event operations delegate through the object-safe adapter and that query-plane / trajectory capabilities can be consumed through the stack.

## Consequences

### Positive

- Backend selection becomes observable and testable.
- Postgres can be promoted without hidden Turso-only assumptions.

### Negative

- The stack has several capability traits; new storage concerns should join an
  existing concern-based trait where possible instead of creating method-sized
  traits.

### Risks

- Mixed backend configuration can create split-brain platform data. Cutover documentation must set all production stores to Postgres together.

### DST Compliance

- Env var reads happen at startup only and are annotated as external configuration. Simulation remains on `SimEventStore`/`SimPlatformStore`.

## Non-Goals

- This ADR does not remove Redis ephemeral mode.
- This ADR does not add per-tenant Postgres schema routing; row-level tenant columns remain the first implementation.

## Alternatives Considered

1. **Change the runtime `EventStore` trait directly** — rejected because it would churn every store implementation; the object-safe adapter localizes the bridge in `temper-server`.
2. **Continue enum matches with no-ops** — rejected because this caused silent backend degradation.

## Rollback Policy

Set storage env vars back to Turso before cutover. The code keeps Turso selectable while Postgres parity is verified.
