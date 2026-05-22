# ADR-0118: OData Pushdown Planner Engagement Observability

- Status: Proposed
- Date: 2026-05-22
- Deciders: Temper core maintainers
- Related:
  - ADR-0117: OData Pushdown Sparse Page Planning
  - `crates/temper-server/src/odata/read.rs`
  - `crates/temper-server/src/odata/read_support/pushdown_page.rs`

## Context

ADR-0117 introduced a sparse page planner for OData collection reads that are already narrowed by SQL filter pushdown. The intended hot shape is:

`/tdata/SessionEntries?$filter=SessionId eq '{session}'&$orderby=Sequence desc&$top=1`

Production rollout to `sha-f4bca0d` was version-correct, but live after data did not improve. Datadog spans showed `filter_pushdown=true`, `candidate_count` around `703-1141`, and `returned_count=1`, while `pushdown_sparse_page=false`, `pushdown_sparse_probe_count=0`, and `pushdown_page_count=0`.

The current span fields prove that the planner did not engage, but they do not explain why. That makes performance work too dependent on source-code inference after deployment. The planner also sits next to catalog coverage and repair logic, so a safe fix must preserve correctness and projection repair behavior while making the fast-path decision auditable.

## Decision

Temper will make sparse pushdown page planning a measured, reasoned decision rather than a boolean-only outcome.

### Emit Planner Skip Reasons

The OData read span will record a low-cardinality `pushdown_sparse_skip_reason` field. Planned values include:

- `not_checked`
- `engaged`
- `no_filter_pushdown`
- `no_filter`
- `has_expand`
- `empty_candidates`
- `page_not_reduced`
- `missing_required_fields`
- `catalog_coverage_missing`

**Why this approach**: boolean fields are useful for aggregation but insufficient for production diagnosis. A skip reason lets Datadog distinguish unsupported query shapes from bugs in the engagement conditions.

### Make Coverage Checks Sparse

Coverage checks must not load full catalog JSON state. The read path may still compare all known entity IDs against catalog presence to preserve repair behavior for rows that are missing from the field index, but that check must use the existing selected-catalog path with only the minimal field set needed to prove row presence.

**Why this approach**: correctness requires repairing missing catalog rows that may match the query. The production problem was not the existence of the check; it was that the check pulled full `fields` and `state` JSONB before the sparse planner could run. Reusing the selected-catalog capability avoids expanding the storage trait in this repair PR.

### Regression-Test The Production Query Shape

Focused tests must prove that the `SessionEntries` shape with a translated filter, descending order, and `$top=1` enters the sparse planner and records enough telemetry to verify engagement.

**Why this approach**: the previous helper-level tests validated the page-selection logic, but did not prove the top-level OData read path attempted the planner under production-like conditions.

## Rollout Plan

1. **Phase 0 (Immediate)** — Add planner skip-reason telemetry, correct the engagement guard, and add focused tests.
2. **Phase 1 (Rollout)** — Ship through TemperPaw, deploy to Railway with a new version tag, and repeat the same live before/after query batch.
3. **Phase 2 (Acceptance)** — Count the slice as a latency win only if Datadog after spans show `pushdown_sparse_page=true` and live p50/p95 improve materially for the fixed query shape.

## Readiness Gates

- The production query shape has a red/green regression test.
- `cargo fmt`, focused tests, `cargo check -p temper-server`, and clippy pass.
- Live after batch records request timings for the same SessionEntries query family.
- Datadog after spans include `pushdown_sparse_skip_reason` and prove planner engagement or an explicit safe skip.

## Consequences

### Positive

- Future OData planner regressions become self-explaining in Datadog.
- The sparse page planner can be accepted or rejected with direct production evidence.
- Sparse coverage keeps correctness repair without forcing full JSONB catalog state onto the hot read.

### Negative

- Adds another span tag and a little planner surface area.
- Requires one more rollout cycle before PERF-040 can be considered a measured win.

### Risks

- Sparse coverage still scans the candidate ID list that needs a coverage proof. Very large sets can remain expensive, but the query no longer pulls full `fields` and `state` payloads before the planner can run.

### DST Compliance

- The change touches `temper-server` read planning only.
- No wall-clock reads, random IDs, filesystem/network side effects, global mutable state, or nondeterministic collections are introduced.
- Existing deterministic `BTreeSet`/`BTreeMap` usage is preserved.

## Non-Goals

- Replacing the OData SQL translator.
- Changing public OData semantics.
- Skipping projection correctness repair when candidate catalog coverage is genuinely missing.
- Optimizing unrelated full-session execution paths in this PR.

## Alternatives Considered

1. **Only patch the suspected guard** — Rejected because production would still lack a skip reason if another condition blocks engagement.
2. **Always sparse-plan every filtered read** — Rejected because `$expand` and unreduced pages should keep using the existing safer materialization path.
3. **Trust live timing without Datadog proof** — Rejected because the goal requires both correctness and observability-grade evidence.

## Rollback Policy

Disable or revert the sparse planner branch. The existing full-candidate materialization fallback remains intact and is the rollback behavior.
