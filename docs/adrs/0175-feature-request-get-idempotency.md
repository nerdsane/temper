# ADR-0175: Feature-Request GET Is a Pure Read

- Status: Accepted
- Date: 2026-07-14
- Deciders: Temper core maintainers
- Related:
  - Linear: ARN-240
  - ADR-0025: Evolution records as entities
  - ADR-0013: Evolution loop agent integration
  - `crates/temper-server/src/observe/evolution/operations.rs`
  - `crates/temper-server/src/observe/evolution/operations/reconcile.rs`
  - `crates/temper-store-turso/src/store/evolution.rs`
  - `crates/temper-store-postgres/src/platform.rs`

## Context

`GET /observe/evolution/feature-requests` was state-changing. On every read it:

1. Regenerated feature requests from trajectory evidence.
2. Upserted the legacy feature-request projection.
3. Created a new IOA `FeatureRequest` entity with `next_system_entity_id("FR")`.

Polling, browser refresh, caches, and retries therefore materialize duplicate identities and widen dual-store divergence. A nominally safe HTTP GET was neither idempotent nor side-effect-free.

## Decision

### Sub-Decision 1: GET is a bounded pure query

`handle_feature_requests` only authorizes and lists durable projections. It does not load trajectories for generation, does not upsert metadata, and does not create system entities.

**Why this approach**: HTTP GET must be safe for observers. Materialization belongs on an explicit write path.

### Sub-Decision 2: Materialization is an authorized write command

Feature-request generation and durable projection run from the evolution write path (`POST /api/evolution/sentinel/check` today; any future explicit materialize command follows the same contract). The GET surface remains read-only even if generation later moves to a durable worker.

### Sub-Decision 3: Stable identity from tenant + evidence + generator version

Entity identity is derived, not allocated:

```text
stable_id = "FR-" || sha256(json({
  tenant,
  generator_version,
  category,
  description,
  frequency,
  trajectory_refs (sorted)
}))
```

Materialization dispatches `CreateFeatureRequest` with an idempotency key `feature-request:{stable_id}` so concurrent and retry writes converge on one entity.

A deliberate generator/evidence revision changes the hash and creates a new identity (explicit version semantics). Order of evidence arrival must not.

### Sub-Decision 4: Projection upsert preserves human review state

Metadata upserts update generated fields (`category`, `description`, `frequency`, `trajectory_refs`) only. `disposition` and `developer_notes` are human-owned and are never clobbered by regeneration.

### Sub-Decision 5: Legacy projection reconciliation is tenant-safe

Legacy rows using the old `FR-YYYY-<12 hex>` id shape may be reconciled into the stable id when:

- evidence revision matches (category, frequency, description prefix, sorted trajectory refs), and
- trajectory refs resolve unambiguously to a single tenant matching the materializing tenant.

Ambiguous multi-tenant legacy rows are left alone. Review notes merge without dropping canonical note bytes; duplicate note components are deduplicated.

## Rollout Plan

1. **Phase 0 (This PR)** — Pure GET; stable idempotent materialization on sentinel; legacy reconcile; store delete + review-preserving upsert; regression tests.
2. **Phase 1 (Follow-up)** — Optional dedicated authorized materialize command / worker if generation should leave the sentinel hot path.
3. **Phase 2** — Drop dual projection once IOA entity store is sole source of truth (related: ARN-218).

## Consequences

### Positive

- Repeated and concurrent GETs perform no writes.
- Same evidence revision materializes exactly one durable identity.
- Generator version changes create explicit new identities.
- Human disposition/notes survive rematerialization and upgrades.

### Negative

- Fresh feature requests no longer appear as a side effect of opening Observe; callers must run sentinel (or a future materialize command).
- Legacy ambiguous multi-tenant rows require manual cleanup if refs cannot prove ownership.

### Risks

- Callers that relied on GET to populate data will see empty lists until materialization runs. Mitigation: document the write path; sentinel already runs in the evolution loop.
- Hash identity couples to description text; model changes must bump `generator_version`.

### DST Compliance

- Stable ids use SHA-256 over a sorted deterministic JSON payload (no wall clock / random UUID on the write path for identity).
- Trajectory ref sorting uses deterministic order.
- Idempotent dispatch keys are pure functions of the stable id.

## Non-Goals

- Removing the legacy metadata projection entirely (dual-store cleanup remains related work).
- Changing FeatureRequest IOA state machine transitions or Observe UI beyond the read contract.
- Cross-tenant feature-request listing redesign.

## Alternatives Considered

1. **Keep generation on GET but cache by evidence hash** — Still makes GET a writer; races and dual-store growth remain. Rejected.
2. **Soft-delete duplicates after GET** — Leaves write-on-read and repair complexity. Rejected.
3. **UUID entity ids with unique constraint on evidence hash** — Workable, but content-addressed stable ids make retries and restarts simpler without a separate index. Prefer stable id as primary.

## Rollback Policy

Revert the PR: restore generation inside `handle_feature_requests` and the previous non-preserving upsert SQL. Durable rows written with stable ids remain valid entities; legacy reconcile deletes would need manual restore from backups if rolled back after production cleanup.
