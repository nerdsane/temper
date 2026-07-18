# ADR-0070: Postgres Multi-Tenant Isolation

- Status: Accepted
- Date: 2026-04-29
- Deciders: Temper core maintainers
- Related:
  - ADR-0160: tenant isolation
  - ADR-0065: Postgres Platform Store and Canonical Schema
  - ADR-0066: StorageStack Backend Selection
  - `crates/temper-store-postgres/src/schema.rs`

## Context

Turso supported database-per-tenant routing for workloads that needed hard isolation. The Postgres cutover uses one Railway Postgres service, so the isolation contract must be explicit instead of hidden in deployment shape.

## Decision

Postgres uses row-level tenancy for the first Railway cutover. Tenant-scoped tables carry a `tenant` column and the schema enables RLS policies that compare each row against `current_setting('app.current_tenant', true)`.

Application code still binds tenant predicates in SQL. RLS is defense in depth, not the only guard. Production must run with a non-superuser database role before RLS is considered enforced.

Schema-per-tenant is deferred until a tenant demonstrably needs independent backup/restore, independent regional placement, or separate operational ownership. If added, it must be selected through `StorageStack`/tenant routing, not ad hoc query branching.

## Readiness Gates

- Every tenant-scoped Postgres table is covered by `ENABLE ROW LEVEL SECURITY` and a `tenant_isolation` policy.
- Cutover role is not a Postgres superuser or table owner that bypasses RLS.
- Any transaction that relies on RLS sets `app.current_tenant` before executing tenant-scoped SQL.
- OData collection reads and platform metadata reads include tenant predicates in normal application SQL.

## Consequences

### Positive

- One Railway Postgres instance can serve all current tenants.
- Cross-tenant operational dashboards remain simple because data is in one database.
- RLS provides a database-side backstop for missed tenant predicates.

### Negative

- Per-tenant restore is coarser than database-per-tenant Turso until schema-per-tenant routing exists.
- RLS must be verified with the actual production role; local superuser tests can give false confidence.

### DST Compliance

This is a storage deployment decision. Simulated stores keep explicit tenant keys and do not depend on Postgres RLS.

## Rollback Policy

Rollback is selecting Turso storage before production cutover. After cutover, rollback is the maintenance-window contract in the Postgres runbook: stop writes, point storage env vars back to Turso, and restart.
