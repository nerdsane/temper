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

- `restore_registry_from_postgres` and `restore_registry_from_turso` are the
  paths the live server runs. Both parsed and registered tenants with `?`, so
  one corrupt CSDL row aborted boot for every tenant.
- `restore_registry_from_platform_store` was fault-isolating, but only the DST
  harness used it. Its documentation incorrectly called it the live path.

The first ARN-190 change shared the per-tenant loop, but it still gave the
backends different side effects: Postgres and Turso retained corrupt committed
rows, while the simulation-only platform-store path deleted them to satisfy P1.
That destroyed diagnostic evidence in DST and tested behavior production did
not have. The only live indication of quarantine was a warning log.

## Decision

### One fault-isolating restore core

One private `restore_grouped_specs<R>` owns the per-tenant loop for every
backend. It parses and merges CSDL, registers the tenant, and continues after a
tenant-local failure. Missing CSDL, invalid CSDL, and registration failures each
quarantine only the affected tenant.

The core returns `RegistryRestoreHealth`: a restored-spec count plus a
deterministic map of quarantined tenants and affected entity types. Each entry
carries the committed spec version, stable reason (`missing_csdl`,
`invalid_csdl`, or `registration_failed`), source kind, parser line/column when
available, and a bounded diagnostic. The backend-neutral `PlatformStore` row
now carries verification state and tenant constraints, so Postgres, Turso, and
simulation all run this exact production path rather than layering parallel
adapter-specific restore implementations around it.

**Why this approach**: sharing the control flow makes production and simulation
fault isolation identical by construction, while a typed report prevents an
error from being reduced to an untestable log string.

### Quarantine and retain on every backend

Postgres, Turso, and the trait-based platform store all retain committed rows
that cannot be activated. Auto-deleting on a parse error could destroy
recoverable evidence, such as a row needing a format migration. Every backend
retries the retained row on the next boot and continues serving healthy tenants.

P1 is defined as: every committed spec is either active in the registry or is
listed in structured restore quarantine health. Unaccounted store/registry
divergence still fails; intentional degraded operation does not require data
deletion to manufacture equality.

**Why this approach**: fault isolation must not become silent data loss, and
DST must exercise the same retention contract as live storage.

### Quarantine is durable and versioned

Postgres migration 0013 and Turso startup schema create
`registry_restore_quarantines`, keyed by
`(tenant, entity_type, spec_version, constraint_version)`. The absent-constraint
snapshot is represented durably as version zero. Each boot atomically resolves
the prior active snapshot and re-opens the failures it still observes. A unique
partial index permits only one unresolved identity per tenant/entity. Repeated
failures update `last_observed_at`; operator acknowledgment is preserved only
for the same spec-plus-constraint identity. Either source version changing
creates an unacknowledged record. Resolved history remains in the table while
active queries return only unresolved rows.

Every replacement and resolution also carries the complete committed source
manifest in scope: every `(tenant, entity_type, spec_version)` plus exact
constraint presence/version for every represented tenant. Postgres compares
that manifest while holding short control-plane locks against source writes and
other quarantine mutations; Turso uses one immediate transaction; simulation
uses the same comparison under its store mutex. A sibling insertion or removal
therefore invalidates the whole operation, including a healthy boot that would
otherwise clear an older quarantine without writing a new failure row.

The quarantine transaction is part of the restore contract. If Temper cannot
persist and re-read the active snapshot, startup returns an error rather than
claiming a tenant was safely quarantined when the only evidence is a log line.

**Why this approach**: retained source is evidence of what failed, while the
versioned quarantine record is evidence that activation was deliberately
withheld. Both are required to distinguish safe degraded operation from silent
registry/store drift.

### Degraded boot is queryable state

`SpecRegistry` retains aggregate restore health for the current process.
`/observe/health` returns it as `registry_restore` and reports overall status
`degraded` while any tenant is quarantined. Health exposes at most 64 entries,
plus exact totals and a `truncated` flag; parser detail is excluded. The
Prometheus surface exports only two label-free gauges: quarantined specs and
quarantined tenants. This prevents untrusted tenant/type names from becoming
unbounded metric cardinality.

**Why this approach**: warnings are not state. Operators and invariant tests
need to distinguish deliberate quarantine from an accidentally omitted
registration.

### Authenticated repair workflow

An administrator operating behind the trusted bearer-auth edge can:

- list active records for one tenant (bounded to 128 records, with diagnostic
  detail and truncation metadata);
- acknowledge one tenant/entity record without hiding or resolving it; and
- retry a quarantined tenant after repairing the committed source out of band.

Retry first compiles the complete tenant into a scratch registry. Failure
refreshes only that tenant's durable records. Success resolves the records
in one version-checked storage transaction, then applies the same validated
sources through the live registry's hot-swap path. If any entity or constraint
version drifts, a sibling entity is inserted or removed, or a constraint is
added or removed, the complete resolution transaction rolls back.
The entity named by the route must match either the process-local identity or
an active durable identity; local health alone is never a prerequisite.

Resolution is idempotent for one exact historical quarantine identity. If
replica A already resolved that identity, replica B may validate the exact
current manifest, prove that no newer active quarantine exists, and activate
without restart. This is deliberately not a blanket "no active row" bypass:
the historical identity must exist and the complete source manifest must still
match. A process-local source snapshot also guards the final hot-swap, so retry
does not overwrite a newer registry generation installed in the same process
while storage validation was in flight. The durable source-manifest CAS is the
cross-process linearization point; a source commit after it is a later mutation
and follows the platform's normal spec-propagation contract.

Acknowledgment likewise carries the exact spec and constraint identity the
operator inspected, so a delayed request cannot acknowledge a newer failure.
After the durable update succeeds, the same exact version is marked acknowledged
in process-local health so the serving replica's health response does not
contradict its authenticated repair API. Durable repair records remain the
cross-replica authority; boot and failed-retry reconciliation hydrate the
matching process-local flag from that record. The acknowledgment handler also
hydrates a locally missing or stale identity from a bounded durable read. It
snapshots the exact local spec-plus-optional-constraint identity before the
first await and applies the durable record only through an optimistic local
compare-and-set. Any in-flight change is preserved without pretending the two
independent version dimensions have a total ordering. An ordinary in-memory spec
registration cannot clear degraded health ahead of this durable compare-and-set.
A client-supplied `x-temper-principal-kind: admin` header is not sufficient: the
kernel requires a `TrustedIngressPrincipal` extension inserted by successful
bearer authentication, and a resolved agent credential remains non-administrative
even if it also supplies admin-shaped headers. Activation uses the exact tenant
constraint snapshot compiled during retry; it never re-reads unvalidated
constraints after durable resolution.

## Rollout Plan

1. Land the shared report and uniform retention semantics with deterministic
   restart/recovery coverage.
2. Alert on `temper_registry_restore_quarantined_specs` or health status
   `degraded`.
3. Inspect and acknowledge the record, repair retained source in place, then
   invoke retry (or restart) and confirm health returns to `healthy`.

## Consequences

### Positive

- A corrupt tenant cannot fail boot for healthy tenants.
- Corrupt committed rows remain available for diagnosis and repair everywhere.
- Production and DST use the same restore, retention, health, and recovery
  contract.
- Repeated restarts preserve the quarantine; repairing the row recovers without
  a manual delete.
- Real Turso and Postgres adapter contract tests prove the same committed-only,
  durable quarantine behavior as the DST.
- Two `ServerState` instances sharing one Turso store prove that a stale replica
  can reconcile an already-resolved exact identity without restart.
- The inverse replica direction is covered too: a process that started healthy
  can discover an exact durable quarantine through retry and reconcile its
  process-local health instead of rejecting solely on stale local state.

### Negative

- A degraded server intentionally has committed rows absent from the live
  registry, so consumers must use the structured P1 definition rather than raw
  store/registry equality.
- Retained bad rows retry and emit a warning on each boot until repaired.
- A quarantine persistence failure fails startup rather than allowing an
  unaccounted degraded state.
- Postgres restore/repair briefly takes shared source-table locks and a
  quarantine-mutation lock to make complete-set comparison atomic. These are
  bounded control-plane operations but can briefly queue concurrent spec writes.

### Risks

- An operator could ignore degraded health and leave a tenant unavailable.
  Stable bounded health fields support alerting and name the exact entity types.
- A different replica may commit a later spec version after repair's durable CAS.
  That later write is not retroactively part of the completed repair; its normal
  mutation/propagation path remains responsible for activating the new version.
- Parser details could expose source fragments. The health API carries only a
  stable category; bounded detail is limited to protected logs and the
  authenticated repair API.
- The existing Turso single-row spec write path stages `committed = 0` by
  overwriting the prior row, while `/api/specs/load-dir` does not complete a
  batch commit. Startup cleanup can therefore delete a successfully hot-loaded
  row and its prior last-known-good source. Fixing that requires a separate
  atomic staged/batch commit design and restart E2E; ARN-190 deliberately does
  not disguise it as part of quarantine recovery.

### DST Compliance

- Restore health uses `BTreeMap` and `BTreeSet`, giving deterministic iteration
  and serialization.
- The shared contract performs no wall-clock, random, threaded, or
  backend-specific cleanup work.
- Simulation timestamps are deterministic literals; durable production
  timestamps are assigned inside the storage adapter and are not observable by
  simulation logic.

## Non-Goals

- Automatically rewriting or repairing corrupt source is not part of startup.
- Serving a tenant whose registry failed to compile is not permitted.
- Redesigning all spec mutation endpoints around an atomic last-known-good
  staging transaction is tracked separately; this ADR only defines how boot
  handles the committed snapshot it receives.

## Alternatives Considered

1. **Delete corrupt rows** — rejected because it destroys evidence and made DST
   semantics diverge from production.
2. **Keep warning logs or process memory only** — rejected because neither can
   survive restart or distinguish expected quarantine in invariants.
3. **Abort the whole process** — rejected because one tenant must not control
   availability for healthy tenants.

## Rollback Policy

Reverting the structured health surface is safe, but rollback must not restore
backend-specific deletion. Retained rows remain valid input for a prior binary;
an operator may explicitly delete one only after preserving evidence.
