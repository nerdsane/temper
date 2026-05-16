# ADR-0089: Cedar AuthZ Policy Candidate Index

- Status: Proposed
- Date: 2026-05-15
- Deciders: Temper core maintainers
- Related:
  - ADR-0046: System authorization via explicit Cedar policy
  - ADR-0081: Latency observability acceleration program
  - ADR-0084: AuthZ latency phase instrumentation
  - ADR-0088: Native File value write fast path
  - `crates/temper-authz/src/engine/mod.rs`
  - `crates/temper-authz/src/metrics.rs`

## Context

ADR-0084 made Cedar authorization measurable by splitting each authorization
check into bounded phases. The first production latency improvement from
ADR-0088 removed the File `$value` WASM/secret/blob adapter loop and reduced a
live upload server span from about 3.16 seconds to about 353 ms.

After that win, production traces still show repeated
`entity.authorize_with_context` spans around 36-50 ms. The phase metrics show
that the `authorizer` phase dominates those spans: request construction,
context conversion, and entity construction are small. A live policy inventory
for the production tenant on 2026-05-15 reported 136 enabled Cedar policies and
about 309 KB of Cedar text. Many of those policies are app, issue, or
decision-specific and are structurally irrelevant to a given request.

This is not a mission-level limitation of Temper. Temper should keep explicit
Cedar governance, tenant isolation, and auditability. The avoidable cost is that
each request currently evaluates against the tenant's whole compiled
`PolicySet`, even when Cedar policy scope constraints already prove that most
policies cannot match the current principal, action, or resource.

## Decision

### Sub-Decision 1: Build a Scope-Safe Candidate Policy Set

Temper will construct a smaller candidate `PolicySet` for each authorization
request before calling Cedar's `Authorizer`. The candidate set is built from the
tenant's loaded policies and contains every policy whose public Cedar scope
constraints may match the current request.

The filter may drop a policy only when its principal, action, or resource scope
constraint is impossible for the current request:

- `principal == X` only matches the exact principal UID.
- `principal is T` only matches the principal entity type.
- `action == A` only matches the exact action UID.
- `action in [A, B]` only matches actions listed in the scope.
- `resource == R` only matches the exact resource UID.
- `resource is T` only matches the resource entity type.

Hierarchy-bearing constraints such as `principal in Group::"x"` and
`resource in Folder::"x"` are treated conservatively unless the current
no-parent entity store proves an exact non-match. The implementation must favor
including too many policies over excluding a policy that Cedar could have used.

**Why this approach**: Cedar already exposes policy scope constraints through a
stable public API. Using those constraints gives us a correctness-preserving way
to reduce candidate volume without interpreting Cedar expressions ourselves.

### Sub-Decision 2: Do Not Cache Authorization Decisions

This change will not cache allow/deny decisions. It will not memoize decisions
by principal, resource, or context. It will only reduce the policy set submitted
to Cedar for a single request.

**Why this approach**: Temper's policies can depend on current entity state,
request context, resource attributes, and human-approved governance changes.
Decision caching risks stale authorization. Candidate selection is safer because
it depends only on policy scope and request identity, while Cedar still evaluates
the full `when` and `unless` clauses for included candidates.

### Sub-Decision 3: Preserve Forbid Semantics and Diagnostics

Candidate selection applies to permit and forbid policies identically. Broad
forbids remain broad. Resource, principal, and action-scoped forbids are
included whenever their scopes may match. Filtered policy sets preserve original
policy IDs so Cedar diagnostics continue to report meaningful policy IDs.

**Why this approach**: A fast authorization path is only acceptable if it keeps
Cedar's deny-overrides semantics and remains explainable in production.

### Sub-Decision 4: Instrument Candidate Volume

The authorization metrics will record the full policy count and candidate policy
count for each evaluation with low-cardinality labels. Datadog can then show
whether authorization latency follows candidate volume, and whether future
policy growth is creating avoidable CPU cost.

**Why this approach**: We need to separate unavoidable Cedar expression cost
from preventable candidate-set growth. Candidate-count metrics make that visible
without adding high-cardinality policy IDs to metrics.

## Rollout Plan

1. **Phase 0 (Immediate)** - Add the candidate filter in `temper-authz`, add
   equivalence tests that compare full `PolicySet` and candidate `PolicySet`
   results, and add candidate-count metrics.
2. **Phase 1 (TemperPaw pin)** - Bump TemperPaw to the merged Temper commit and
   run local TemperPaw tests.
3. **Phase 2 (Production proof)** - Deploy to Railway, run the same live File
   `$value` and representative OData operations, and compare
   `entity.authorize_with_context` plus Cedar phase metrics in Datadog.
4. **Phase 3 (Follow-up)** - If candidate filtering is insufficient, investigate
   policy compaction, policy-shard materialization, or safe candidate-set caches.
   Authorization-decision caches require a separate ADR.

## Readiness Gates

- `cargo test -p temper-authz` passes.
- New tests prove allow, deny, forbid, no-match, diagnostics, and tenant reload
  behavior match full Cedar evaluation.
- A synthetic large-policy test demonstrates irrelevant policies are excluded
  from candidates while the decision is preserved.
- `cargo check -p temper-server` passes after the authz crate change.
- Production Datadog traces show candidate-count metrics and reduced
  `authorizer` phase duration for normal operations.
- Live end-to-end requests still pass with correct authorization behavior.

## Consequences

### Positive

- Authorization latency becomes proportional to relevant policy scope, not total
  tenant policy history.
- Policy growth becomes observable through candidate-count metrics.
- Temper keeps explicit Cedar governance and auditability.

### Negative

- Each request allocates a filtered `PolicySet` before calling Cedar.
- The authz engine takes on Cedar-scope matching logic that must track the
  public `cedar-policy` API.

### Risks

- An unsound filter could drop a forbid or permit policy that Cedar would have
  matched. Mitigation: filter only on impossible public scope mismatches, keep
  hierarchy-like constraints conservative, and test candidate evaluation against
  full evaluation.
- Candidate-set construction may not beat full evaluation for tiny policy sets.
  Mitigation: measure candidate count and phase duration in production; add a
  threshold or candidate-set cache only if evidence requires it.

### DST Compliance

This decision is implemented in `temper-authz`, outside the simulation-visible
actor crates. If server integration code is touched, it must keep existing
deterministic state handling and avoid wall-clock or random behavior. Metrics
recording remains outside simulation semantics.

## Non-Goals

- No authorization-decision caching.
- No policy rewrite, policy compaction, or semantic simplification.
- No schema-specific hardcoding of Temper application entity names.
- No change to Cedar policy authoring or human approval workflow.

## Alternatives Considered

1. **Evaluate the full tenant `PolicySet` forever** - Safest but leaves
   authorization cost coupled to total policy volume and makes policy-history
   growth a latency hazard.
2. **Cache allow/deny decisions** - Potentially faster, but risks stale
   authorization when policies, resource attributes, or context changes.
3. **Split policies into app-specific stores manually** - Useful long term, but
   it changes policy-loading semantics and still needs a scope-safe candidate
   story for decision-specific policies.
4. **Replace Cedar for hot paths** - Rejected. Temper's mission depends on
   explicit, inspectable Cedar governance.

## Rollback Policy

The implementation is local to `temper-authz`. Rollback by reverting the
candidate filter and candidate-count metrics so authorization again submits the
full tenant `PolicySet` to Cedar.
