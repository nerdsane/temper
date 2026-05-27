# ADR-0134: Query Plane Read Contract

- Status: Proposed
- Date: 2026-05-27
- Deciders: Temper core maintainers
- Related:
  - ADR-0077: Catalog-First OData Materialization
  - ADR-0110: Bounded Catalog Shadow Read Probes
  - ADR-0111: Full-State Catalog Fast Read
  - ADR-0114: Bounded Projection Replay Parity Probe
  - ADR-0115: OData Selected Catalog Projection
  - ADR-0119: Bounded Query Projection Probes And Pages
  - PR #278: Cedar authorization for TData reads
  - `crates/temper-server/src/odata/read.rs`
  - `crates/temper-server/src/odata/query_plane_read.rs`
  - `crates/temper-server/src/storage/query_plane_read.rs`

## Context

The OData/projection read path accumulated several locally correct fast paths:
SQL filter pushdown, catalog coverage repair, selected/full catalog
materialization, bounded actor fallback, sparse-page observability labels,
Cedar row authorization, shadow checks, and replay parity probes. After PR
#278, row authorization also affects paging and `$count`, so route-local
planning can no longer safely re-enable storage-paged results unless the
authorization boundary is explicit.

The problem is not any one optimization. The problem is that route code decides
read strategy, fallback reasons, budgets, catalog correctness, authorization,
and telemetry in one function while replay parity has its own catalog loading
branch. That makes every new projection read improvement reopen the same files.

## Decision

### OData Owns Query Execution

Create one server-side OData query-plane read API. It accepts tenant, entity
type, entity set name, parsed OData query options, Cedar security context,
translated filter capability, and explicit read budgets. It returns materialized
entities, count, selected strategy, typed fallback reason, coverage counts,
shadow-check counts, and telemetry values.

`handle_entity_set` remains responsible for routing concerns only: resolve the
entity type, call the read API, expand final entities, and build the OData
response.

**Why this approach**: OData paging, `$count`, `$select`, `$expand`, and Cedar
row authorization are a user-facing query contract. Keeping that contract in one
OData module prevents storage helpers or route branches from independently
deciding response semantics.

### Fallback Reasons Are Typed

Fallback and skip reasons become enum variants with stable span labels. Current
low-cardinality labels such as `no_filter_pushdown`,
`cedar_row_authorization`, and `fallback_candidate_budget` remain stable unless
a follow-up ADR intentionally renames them.

**Why this approach**: Datadog dashboards and local tests should reason about a
closed set of reasons instead of scattered string literals.

### Replay Parity Shares The Substrate, Not The OData Planner

Replay parity reuses bounded catalog/listing primitives and typed catalog-load
outcomes. It does not call the OData query read API.

**Why this approach**: Replay parity is a diagnostic correctness verifier. It
asks whether the projected catalog matches authoritative event replay. OData is
an online query executor that asks which authorized rows and response shape to
return. Sharing bounded storage primitives keeps the architecture clean without
turning the OData request API into a generic "do everything with projections"
interface.

## Rollout Plan

1. **Phase 0 (Immediate)** - Add the ADR, shared bounded catalog helpers, the
   OData query-plane read module, and characterization tests. Preserve public
   OData behavior after PR #278.
2. **Phase 1 (Follow-up)** - Revisit native storage-paged reads only if the
   planner can prove row authorization cannot affect page membership, or if
   storage can evaluate the same authorization predicate.
3. **Phase 2 (Production proof)** - Verify local tests, local live OData reads,
   deployment health, and Datadog spans/metrics for fallback reasons and
   materialization counts.

## Readiness Gates

- `handle_entity_set` no longer owns query strategy branches.
- Filtered/ordered OData reads remain bounded and preserve Cedar row
  authorization before paging/count.
- Replay parity uses shared bounded catalog helpers without depending on OData
  response planning.
- Existing OData behavior and tests remain green.
- Datadog span fields stay present with stable low-cardinality labels.

## Consequences

### Positive

- Query strategy, budget, fallback reason, coverage, and telemetry are testable
  in one place.
- Replay parity and OData share catalog-read correctness handling without
  coupling their high-level contracts.
- Future native page optimization must pass through the authorization-aware
  planner instead of bypassing it from route code.

### Negative

- The first slice does not restore native SQL page pushdown for authorized
  external reads.
- The read API carries OData-specific concepts and should not be treated as a
  storage trait.

### Risks

- Moving logic may accidentally change `$count` or `$select` behavior. Mitigate
  with characterization tests before and after extraction.
- Telemetry dashboards may depend on exact labels. Preserve labels in this PR
  and document intentional renames separately.

### DST Compliance

- Changes are in `temper-server`, a simulation-visible crate, but they do not
  introduce wall-clock time, randomness, filesystem access, or background
  scheduling.
- Existing production-only shadow/replay duration annotations remain unchanged.

## Non-Goals

- Do not redesign `QueryPlaneStore` in this PR.
- Do not make replay parity an OData query.
- Do not re-enable native storage-paged OData results until authorization can be
  proven compatible with paging and count.

## Alternatives Considered

1. **One universal query API for OData and replay parity** - Rejected. It mixes
   user-facing response planning with diagnostic drift verification.
2. **Storage trait redesign first** - Rejected. Existing storage primitives are
   sufficient for this consolidation and a trait rewrite would enlarge the
   blast radius.
3. **Keep route-local branches** - Rejected. This is the source of repeated
   churn and makes fallback semantics hard to audit.

## Rollback Policy

Revert the OData read-module extraction and keep the ADR as a record of the
failed attempt. Because storage traits are not redesigned, rollback is limited
to server routing/read helper code.
