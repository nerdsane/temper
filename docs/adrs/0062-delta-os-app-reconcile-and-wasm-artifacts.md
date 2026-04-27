# ADR-0062: Delta OS-App Reconcile and WASM Artifacts

- Status: Accepted
- Date: 2026-04-27
- Deciders: Temper core maintainers
- Related:
  - ADR-0027: OS App Catalog
  - ADR-0029: Temper Filesystem
  - ADR-0032: Platform Store Trait and Sim Platform DST
  - ADR-0057: Native Immutable File Read Plane
  - ADR-0060: Bounded Warm Restart and Digest-Aware App Reconcile
  - `crates/temper-platform/src/os_apps/reconcile.rs`
  - `crates/temper-platform/src/os_apps/mod.rs`
  - `crates/temper-store-turso/src/store/specs.rs`
  - `crates/temper-store-turso/src/store/wasm.rs`

## Context

TemperPaw production deploys showed that digest-aware reconcile was still too coarse after a dependency update rebuilt bundled WASM apps. The durable app bundle digest changed, so startup entered the install path for every affected startup app. That path then re-persisted every app spec, re-upserted every WASM module, and re-bootstrapped app content serially before readiness.

Datadog traces made the shape of the problem clear:

- `turso.upsert_specs_and_commit` held almost no CPU but waited tens of seconds per transaction under write contention.
- `turso.upsert_wasm_module` also held almost no CPU but waited tens of seconds while repeatedly writing module bytes into SQL.
- A WASM-only rebuild forced unrelated specs, policies, agents, skills, ADRs, system files, and seed data through the install path.

This is not a Turso feature problem by itself. The platform was asking the database to do unnecessary write transactions and was storing bulky immutable module content in the SQL metadata row.

## Decision

### 1. Reconcile plans are component-aware

The durable installed-app row already records subdigests for specs, policies, WASM, app content, and seed data. Reconcile must compare each subdigest and build an explicit plan:

- unchanged specs and policies skip spec persistence, verification bootstrap, policy reload, and cross-invariant persistence
- unchanged WASM skips module persistence, registry re-registration, and eager compilation
- unchanged app content skips App, `APP.md`, agents, skills, system files, and ADR bootstrap
- unchanged seed data skips seed entity creation

The full `bundle_digest` remains the public identity for an installed app version, but it no longer implies that every install phase must run.

### 2. Spec persistence is scoped and idempotent

OS-app install must persist only the app specs being reconciled, not reload and rewrite every committed spec for the tenant.

`upsert_specs_and_commit` must be content-hash gated:

- inserting a new spec writes it committed immediately
- an identical spec and CSDL preserves version, verification status, and `updated_at`
- a changed spec increments version and resets verification
- the transaction records the installed app but does not run a broad `UPDATE specs SET committed = 1 WHERE tenant = ?`

This removes avoidable tenant-wide write amplification and makes the transaction proportional to the app's changed spec set.

### 3. WASM module SQL rows are metadata, not the artifact hot path

WASM modules are immutable artifacts addressed by hash. SQL should track tenant/module metadata: module name, hash, size, version, and timestamps. The module bytes are stored through the blob/artifact store under a content-addressed key.

For the current Turso backend, the artifact key is `wasm-modules/{sha256_hash}` in the existing content-addressed blob table. Existing legacy rows with inline `wasm_bytes` remain readable. New writes may keep an empty inline byte column as a schema-compatible pointer row while loading resolves bytes from the artifact store.

Future external object-store adapters may use the same artifact key contract without changing app reconcile semantics.

### 4. WASM upsert is hash-gated before artifact upload

If the existing tenant/module metadata already has the requested hash, `upsert_wasm_module` returns before uploading bytes or updating SQL.

If the hash changes:

- write the content-addressed artifact once
- update the metadata row
- increment version only for a real hash change

This makes repeated startup reconcile of identical modules a metadata read, not a large SQL write.

### 5. Startup readiness remains honest

The platform should not mark readiness until required startup apps are usable. This ADR reduces work on the required path; it does not hide required reconcile work behind `/readyz`.

Bulky optional content repair may move to later entity-driven workflows only when the app marks that work non-critical.

## Consequences

### Positive

- WASM-only rebuilds no longer replay spec, policy, content, and seed bootstrap.
- Spec-only changes no longer rewrite WASM module rows or app content.
- Identical spec and WASM persistence calls become cheap no-ops.
- WASM bytes stop bloating the SQL metadata row for new writes.
- Startup trace spans become component-proportional and easier to explain.

### Negative

- Reconcile has more phase decisions to test.
- WASM loading must resolve legacy inline rows and artifact-backed rows.
- The current Turso blob table is still a SQL-backed artifact implementation until an external object-store adapter is configured.

### Risks

- A mistaken subdigest comparison could skip needed bootstrap work. Tests must cover changed and unchanged component combinations.
- Artifact rows and metadata rows can diverge if artifact write succeeds but metadata write fails. The write order is artifact first, then metadata; orphaned content-addressed blobs are harmless and deduplicated.
- Legacy inline rows must remain readable until all deployments have rewritten module metadata.

## Rollout Plan

1. Add red tests for idempotent spec and WASM writes.
2. Make Turso spec and WASM writes hash-gated and remove broad spec commit writes.
3. Store new WASM module bytes through content-addressed artifact keys while keeping legacy inline rows readable.
4. Add red tests for component-aware OS-app reconcile.
5. Split the install path into spec, policy, WASM, content, and seed phases driven by the reconcile plan.
6. Update TemperPaw to consume the new Temper revision and verify startup traces show bounded reconcile work.

## Non-Goals

- Introducing a separate orchestrator outside Temper primitives.
- Marking `/readyz` healthy before required startup app surfaces are usable.
- Replacing all Turso blob storage with R2/S3 in this change.
- Changing app bundle digest semantics.

## Rollback Policy

The install path can run with an all-phases plan to recover the previous behavior. Artifact-backed WASM loading preserves legacy inline rows, so rolling back code does not invalidate existing module metadata; a rollback may simply rewrite inline bytes again on the next changed module install.
