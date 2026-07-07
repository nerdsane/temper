# ADR-0160: Fault-isolating registry restore shared by every backend

- Status: Accepted
- Date: 2026-07-07
- Deciders: Temper core maintainers
- Related:
  - `crates/temper-server/src/registry_bootstrap.rs` (restore paths)
  - `crates/temper-server/tests/common/platform_harness.rs` (DST harness)
  - `crates/temper-cli/src/serve/bootstrap.rs` (`build_registry`, live boot)
  - ARN-190 (bug), ARN-162 finding `server-actor-storage-2` (P1)

## Context

On boot the server rebuilds its `SpecRegistry` from persisted specs. Three
restore functions existed:

- `restore_registry_from_postgres` and `restore_registry_from_turso` — the
  paths the **live server** actually runs (via `build_registry`). Both call the
  private `populate_registry`, which parsed CSDL and registered each tenant
  inside a per-tenant loop using `?` — so a **single** tenant's corrupt or
  unparsable CSDL returned `Err`, propagated up through `build_registry`, and
  **failed boot for every tenant**. One malformed persisted row (a partial
  write, or a spec-format change) became a whole-server, multi-tenant outage.
- `restore_registry_from_platform_store` — a per-tenant *fault-isolating* path
  that logs, skips, and reconciles a bad tenant, then continues. Its doc comment
  claimed it was "the production code path … used by both the CLI bootstrap and
  the DST harness." That was false: only the **DST harness** called it. The live
  boot never did.

The result was a test-integrity gap on top of the bug: the DST
`platform_invariants` harness exercised the graceful path, while production
shipped the fail-all path. The simulation could never catch the outage.

## Decision

### One fault-isolating restore core

Introduce a single private `restore_grouped_specs<R>` that owns the per-tenant
loop for **every** backend. For each tenant it parses+merges CSDL and registers
the tenant; any failure (missing CSDL, CSDL parse error, or registration error)
**logs and quarantines only that tenant** and continues with the rest. It
returns a `RestoreOutcome { restored_specs, orphaned_specs }` so callers can
reconcile the quarantined `(tenant, entity_type)` pairs as their storage allows.

All three restore functions now funnel through it:

- `populate_registry` (Postgres/Turso) is a thin wrapper that additionally
  restores each spec's persisted verification status via an `on_registered`
  callback. It no longer returns `Result` — a bad tenant can never abort the
  restore.
- `restore_registry_from_platform_store` (DST harness) calls the same core, then
  deletes quarantined specs from the store to preserve P1 (every stored spec has
  a registry entry).

Because the DST harness and the live server now share `restore_grouped_specs`,
the simulation exercises the exact per-tenant isolation logic the CLI ships.

**Why this approach**: The fault-isolation logic lived in one place already
(the platform-store path); the fix is to make it *the* implementation rather
than a parallel copy the live server bypassed. Sharing one core closes the
test-integrity gap by construction instead of by adding a second test.

### Quarantine, don't auto-delete, on the live paths

The Postgres/Turso wrappers log and skip a bad tenant but **do not delete** the
offending rows; the platform-store path reconciles (deletes) them because its
DST invariant P1 requires store/registry agreement. Auto-deleting persisted
production specs on a parse error could destroy recoverable data (e.g. a
spec-format change a migration should handle), so the live paths keep the row
for human inspection and re-quarantine it on each boot until it is fixed.

**Why this approach**: fault isolation must not become silent data loss on the
live server. Logging + quarantine restores availability without discarding the
evidence needed to root-cause the bad row.

## Consequences

- One corrupt tenant CSDL no longer fails boot for healthy tenants — the server
  starts and serves everyone else; the bad tenant is logged and quarantined.
- The DST harness now drives the shared production restore core, so an isolation
  regression is caught in simulation.
- The false doc claim on `restore_registry_from_platform_store` is corrected.
- Quarantined live-path rows persist and re-warn each boot until repaired; this
  is intentional (surfaces the problem rather than hiding it).
