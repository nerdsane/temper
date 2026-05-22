# ADR-0114: Bounded Projection Replay Parity Probe

- Status: Proposed
- Date: 2026-05-21
- Deciders: Temper core maintainers
- Related:
  - ADR-0039: Latency Observability Acceleration Program
  - ADR-0110: Bounded Catalog Shadow Read Probes
  - ADR-0111: Full-State Catalog Fast Read
  - ADR-0113: Catalog Status State Parity
  - `crates/temper-server/src/state/projection_backfill/replay_parity.rs`
  - `crates/temper-server/src/observe/mod.rs`

## Context

Projection-backed reads are now fast enough to serve large collections without hydrating every entity actor, but that only remains correct if the durable query projection keeps matching the event-sourced authority. Recent Taxonomies work proved the risk: the unfiltered read path recovered rows from event/actor fallback while status-filter pushdown trusted an incomplete catalog.

Temper already has a replay-parity verifier that rebuilds authoritative state from the event journal and compares it with `entity_catalog`. Today that verifier is internal and all-scope. Operators need a bounded Observe API surface that can be run repeatedly against one tenant and, when useful, one entity type. The probe must emit run-level Datadog evidence so drift, missing coverage, sequence gaps, and verifier cost can be monitored without waiting for a frontend route to expose bad projection behavior.

## Decision

Add a bounded projection replay parity Observe endpoint:

`GET /observe/projections/replay-parity?entity_type=Taxonomies&limit=100`

The endpoint uses the existing Observe authorization path, resolves tenant through `X-Tenant-Id` or single-tenant fallback, and rejects/limits work through a bounded `limit` parameter. It returns the existing replay parity report plus the applied scope.

### Sub-Decision 1: Bound The Probe By Entity Count

The verifier gains an optional entity type filter and an optional entity limit. Entity pairs are sorted deterministically before filtering and truncation, preserving repeatable probe behavior.

**Why this approach**: Full replay parity is correct but can be expensive in tenants with large journals. A bounded probe lets us run production canaries frequently and reserve full sweeps for lower-frequency maintenance windows.

### Sub-Decision 2: Emit Run-Level Datadog Evidence

In addition to per-entity replay parity metrics, each probe run records:

- total runs by tenant/entity_type/result/source
- run duration
- entities checked
- drifted entities
- missing projection rows
- verifier errors

The Observe handler also records span fields for `checked`, `drifted`, `missing`, `errors`, `clean`, `entity_type`, and `limit`.

**Why this approach**: The existing per-entity metrics answer "what did each comparison find." Run-level metrics answer "is this canary healthy right now" and give dashboards a low-cardinality acceptance signal.

## Rollout Plan

1. **Phase 0 (Immediate)** — Add the bounded state method, Observe endpoint, run metrics, and focused Turso-backed tests.
2. **Phase 1 (Follow-up)** — Roll into TemperPaw, run the endpoint against production Taxonomies, DesignLanguages, and Session-adjacent entities, and record Datadog before/after evidence in the latency dashboard.
3. **Phase 2** — Decide whether to automate the probe as a low-frequency cron or heartbeat once cost is measured.

## Readiness Gates

- The endpoint returns a clean report for a known-good projection fixture.
- The endpoint returns drift/missing details for an intentionally damaged projection fixture.
- The handler enforces a bounded limit and records run-level metrics.
- Production proof records endpoint response, Datadog span fields, and run metric samples.

## Consequences

### Positive

- Projection correctness becomes continuously provable outside user-facing requests.
- Future read-model speed work can be gated by a repeatable production canary.
- Drift diagnosis includes bounded entity examples without high-cardinality metric tags.

### Negative

- Replay parity still consumes store reads and event replay CPU, so it must remain bounded by default.
- Full-tenant sweeps still need an explicit operational window or separate maintenance job.

### Risks

- A too-large limit can compete with production work. Mitigation: cap the Observe endpoint and keep defaults small.
- Operators may treat a clean bounded sample as a full proof. Mitigation: report the applied limit and checked count explicitly.

### DST Compliance

- The implementation touches `temper-server`, a simulation-visible crate, but the endpoint and metrics are production Observe surfaces.
- Entity ordering is deterministic through sorted `(entity_type, entity_id)` pairs.
- Wall-clock timings are used only for production observability metrics and marked with existing `determinism-ok` style comments where new timing is added.

## Non-Goals

- This ADR does not make projection reads eventually consistent by accepting drift.
- This ADR does not replace startup backfill or request-time repair.
- This ADR does not add a production scheduler in the first PR.

## Alternatives Considered

1. **Only rely on request-time shadow reads** — Rejected because shadow checks are sampled and tied to user routes; they cannot prove a projection class before users hit it.
2. **Run full replay parity on every probe** — Rejected because tenants can grow large and replay cost must not become an observability tax.
3. **Store entity IDs in metric labels** — Rejected because entity IDs are high cardinality; examples belong in the JSON report and logs, not metric tags.

## Rollback Policy

Remove the Observe route and run-level metric calls. The existing internal replay parity verifier remains usable by tests and maintenance code.
