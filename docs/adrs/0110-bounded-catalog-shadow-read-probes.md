# ADR-0110: Bounded Catalog Shadow Read Probes

- Status: Proposed
- Date: 2026-05-20
- Deciders: Temper core maintainers
- Related:
  - ADR-0077: Catalog-first OData entity materialization
  - ADR-0082: Projection Correctness Observability
  - ADR-0104: Projection Read Parity And Local Tenant Propagation
  - `crates/temper-server/src/odata/read.rs`
  - `crates/temper-server/src/odata/read_support.rs`
  - `crates/temper-server/src/odata/read_support/shadow.rs`

## Context

Catalog-first OData reads are intended to make projection-backed collection
reads proportional to one indexed catalog query plus JSON response work. After
ADR-0104, production traces showed that `GET /tdata/Taxonomies` can still take
hundreds of milliseconds even when the span reports
`catalog_materialization=true`.

The slow trace shape is not dominated by Postgres. The catalog query for 182
Taxonomy rows took about 12 ms, while the same request trace contained many
`entity.get_tenant_entity_state` and
`entity.get_or_spawn_tenant_actor_with_fields` spans. Those actor reads are
projection shadow checks scheduled for catalog rows. They are valuable for
detecting drift, but a high sample rate can reintroduce the actor fanout that
catalog-first reads were created to remove.

The platform still needs correctness proof. Disabling shadow reads entirely
would make projection drift harder to catch. The problem is unbounded
per-request probe volume, not the existence of probes.

## Decision

Bound catalog shadow probes per collection response.

### Sub-Decision 1: Keep Stable Sampling, Add A Response Budget

Continue using the stable tenant/entity-type/entity-id hash to decide whether a
row is eligible for shadow checking. Add a per-response budget so a single
OData collection read schedules only a small number of actor-backed shadow
checks even if many rows match the sampling hash.

The default budget is intentionally small. Operators can increase it when
running a focused projection audit, but the default production posture should
keep list-read latency governed by the catalog path.

**Why this approach**: Stable sampling preserves deterministic coverage across
requests and deployments. The response budget prevents one large list response
from scheduling dozens of actor loads on the request's hot runtime path.

### Sub-Decision 2: Expose Probe Counts On OData Read Spans

Record the configured per-response budget and the number of scheduled shadow
checks on `odata.entity_set_read` spans.

**Why this approach**: Datadog should make the tradeoff visible. When a list
read is unexpectedly slow, operators can distinguish response JSON size,
catalog DB latency, actor fallback, and correctness-probe fanout.

### Sub-Decision 3: Single Entity Reads Keep Their Existing Probe Behavior

Single-entity catalog reads may still schedule their one eligible shadow check.
The bounded response budget applies to collection materialization, where the
fanout risk exists.

**Why this approach**: A single entity read cannot create the dozens-of-actors
shape observed in production, and it is useful for targeted drift detection.

## Rollout Plan

1. **Phase 0 (Immediate)** - Add the per-response budget, span fields, and
   focused tests for bounded scheduling behavior.
2. **Phase 1 (Rollout)** - Deploy through TemperPaw and rerun the
   `/tdata/Taxonomies` before/after batch against the same production version
   family.
3. **Phase 2 (Continuous Proof)** - Keep a low default request-path sample
   budget and move deeper projection audits to scheduled probes or replay
   parity checks instead of user-facing list requests.

## Readiness Gates

- Local tests prove list materialization does not schedule more than the budget.
- Existing catalog fast-read and projection parity tests still pass.
- `odata.entity_set_read` spans include shadow probe budget and scheduled count.
- Production after proof shows `GET /tdata/Taxonomies` no longer contains a
  large actor shadow-check fanout on normal reads.
- The latency report records before/after client timings and Datadog trace
  evidence.

## Consequences

### Positive

- Preserves projection drift detection while keeping catalog-backed list reads
  fast.
- Prevents observability work from recreating the actor materialization latency
  it was meant to diagnose.
- Adds explicit Datadog evidence for shadow-check cost and volume.

### Negative

- A single list request samples fewer rows than before when the eligible row
  count exceeds the budget.
- Focused audits that need high sample volume must explicitly raise the budget
  or use replay/scheduled correctness probes.

### Risks

- Too low a default budget could delay detection of rare projection drift.
  Mitigation: keep replay parity and continuous projection probes as the
  broader correctness layer; request-path shadow reads are only one signal.
- Operators might mistake a low scheduled count for disabled correctness
  monitoring. Mitigation: expose both the budget and scheduled count on spans
  and document the rollout posture in the latency report.

### DST Compliance

- This touches `temper-server`, a simulation-visible crate.
- The sampling decision remains deterministic for a given tenant/entity/id set.
- The new budget is read from process configuration once, matching existing
  OData environment controls.
- No new wall-clock time, random IDs, filesystem access, or blocking threads
  are introduced.

## Non-Goals

- Do not remove catalog shadow checks.
- Do not redesign projection replay or backfill in this patch.
- Do not change the OData response contract.
- Do not make actor materialization the default path for collection reads.

## Alternatives Considered

1. **Disable shadow reads in production** - Rejected because it removes a useful
   drift signal and conflicts with the correctness requirements for projection
   fast paths.
2. **Leave sampling unbounded** - Rejected because production traces show that
   it can dominate read latency for large collection responses.
3. **Run every shadow check in a detached worker** - Deferred because it needs a
   durable queue and lifecycle policy. A bounded request-path patch is smaller
   and immediately reduces the observed latency risk.
4. **Increase actor materialization concurrency** - Rejected because the actor
   reads are diagnostic probes, not required response data; adding concurrency
   treats the symptom while preserving unnecessary fanout.

## Rollback Policy

If the budget hides important drift in production, raise
`TEMPER_ODATA_CATALOG_SHADOW_READ_MAX_PER_RESPONSE` or revert the budget while
moving the deployment to a focused projection audit window. The span fields can
remain because they are diagnostic and low-cardinality.
