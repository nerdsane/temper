# ADR-0135: Authorized Query Plane Pages

- Status: Proposed
- Date: 2026-05-27
- Deciders: Temper core maintainers
- Related:
  - ADR-0134: Query Plane Read Contract
  - PR #285: Consolidated OData query-plane reads
  - `crates/temper-server/src/odata/query_plane_read/`
  - `crates/temper-server/src/storage/query_plane.rs`
  - `crates/temper-store-postgres/src/query_page.rs`
  - `crates/temper-store-turso/src/store/query_page.rs`

## Context

ADR-0134 moved OData entity-set reads behind one query-plane contract, but the
first implementation still left two correctness and performance gaps.

First, row authorization was represented as a reason to avoid native paging,
but the read-source fallback still truncated candidate IDs at the legacy
10x safety cap and returned a 200 response. Because Cedar row checks run after
materialization, large collections could silently omit authorized rows beyond
that cap and report an incomplete `$count`.

Second, the contract kept the native page capability outside the OData planner.
It used `query_field_index` to fetch all matching IDs, then materialized a
bounded candidate set. That is a cleanup base, but it is not the intended query
plane: filtered and ordered reads should use storage-bounded pages where a
backend can provide them.

## Decision

### Native Pages Are Candidate Pages, Not Final OData Pages

The OData query-plane planner may use `QueryPlaneStore::query_field_index_page`
to fetch ordered candidate IDs from storage. Those IDs are not the final OData
page until each materialized row passes Cedar `read` authorization.

**Why this approach**: Cedar policies may inspect resource attributes and may
deny rows that storage cannot evaluate. Treating storage pages as candidate
pages preserves the authorization boundary while still bounding each backend
round trip.

Storage predicates and ordering are used only when they are known to be
lossless relative to the existing in-memory OData evaluator. Otherwise the
native page source scans catalog candidates and re-evaluates `$filter` after
materialization, or falls back to read-source full proof for orderings outside
the non-nullable CSDL scalar model. Nullable scalar ordering also uses the
read-source proof path until the platform defines one normalized explicit-null
ordering contract for both storage and the in-memory evaluator. Backends must
order missing values like the existing evaluator: present values first for
ascending order, missing values first for descending order.

### Cursor Scanning Must Prove Completeness

For authorized reads, the planner scans candidate pages until it can prove the
requested response:

- Without `$count`, it may stop after it has evaluated enough authorized rows
  to satisfy `$skip` and `$top`.
- With `$count=true`, it must evaluate every matching candidate row so the
  count is exact.
- If the proof would exceed the configured scan budget, the response is a
  typed `413 QueryTooLarge`.

**Why this approach**: Silent truncation is worse than a hard bounded error.
The planner either knows the row-authenticated page/count is complete or tells
the caller that the query needs a narrower predicate or a backend-side policy
capability.

### Read-Source Fallback Also Uses The Proof Contract

When storage cannot provide native candidate pages, the actor/read-source union
is still valid as a candidate source. It follows the same scan budget: no
filter/order/count query may return a 200 response after evaluating only a
truncated prefix.

**Why this approach**: Correctness should not depend on which candidate source
was selected. The fallback is slower, but it must be semantically identical.

### Projection Telemetry Splits Requested From Used

Telemetry distinguishes `$select` being requested from sparse catalog
projection actually being used. The legacy `catalog_select_projection` field
records use, not eligibility. A new `select_requested` field records request
shape.

**Why this approach**: Operators need to know whether bytes were actually
avoided. Recording an optimization as active when Cedar forced full-state
materialization makes dashboards lie.

## Rollout Plan

1. **Phase 0 (Immediate)** - Replace truncating fallback with a cursor/proof
   planner, route native page capabilities through the query-plane contract,
   add Turso parity for native candidate pages, and fix telemetry labels.
2. **Phase 1 (Follow-up)** - Add policy-aware storage authorization only if a
   future Cedar subset can be proven equivalent to resource-attribute checks.
3. **Phase 2 (Production proof)** - Verify local tests, local live reads, CI,
   deployment health, and Datadog span fields for native page use and typed
   budget rejections.

## Readiness Gates

- No row-authorized path returns a 200 response after truncating candidates.
- Native storage pages are used when the backend supports them.
- `$count=true` is exact or the response is `413 QueryTooLarge`.
- Sparse projection telemetry reports actual use, not just `$select` presence.
- Existing public OData response shape is preserved for successful reads.

## Consequences

### Positive

- Large reads fail explicitly instead of returning incomplete data.
- Native candidate paging and sparse probing are represented by the query-plane
  contract instead of route-local branches.
- Datadog span fields reflect the strategy that actually executed.

### Negative

- `$count=true` with row authorization can still be expensive because every
  matching row must be evaluated unless storage learns an equivalent policy.
- Some large unfiltered authorized reads will now return `413` where they
  previously returned incomplete `200` responses.

### Risks

- Cursor scanning can increase storage round trips for heavily denied result
  sets. Mitigate with explicit budgets and telemetry for scanned candidate
  counts.
- Storage and in-memory sort semantics must stay aligned. Characterization
  tests cover non-nullable numeric/string order cases and nullable scalar
  fallback behavior already supported by the query projection.

### DST Compliance

- Changes touch `temper-server`, a simulation-visible crate, but add no wall
  clock time, randomness, filesystem access, or background scheduling.
- Cursor progress is deterministic because candidate IDs are fetched and
  accumulated in stable backend order with deterministic `entity_id` tie-breaks.

## Non-Goals

- Do not push Cedar policy evaluation into SQL in this change.
- Do not expose native candidate pages as final OData pages without row auth.
- Do not redesign the query-plane storage trait beyond the existing page method.

## Alternatives Considered

1. **Return 413 for every row-authorized large read** - Rejected. It is correct
   but does not complete the native page performance goal.
2. **Trust storage pages as final pages for admin requests** - Rejected. Cedar
   policies can still deny admin principals or inspect resource attributes.
3. **Keep legacy truncation with warning telemetry** - Rejected. The response
   contract must not silently omit authorized rows.

## Rollback Policy

Revert the cursor planner and restore ADR-0134 behavior. This is a server-side
read-path change plus Turso page support; no persisted schema changes are
required for rollback.
