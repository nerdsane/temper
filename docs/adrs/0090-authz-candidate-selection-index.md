# ADR-0090: AuthZ Candidate Selection Index

- Status: Proposed
- Date: 2026-05-16
- Deciders: Temper core maintainers
- Related:
  - ADR-0084: AuthZ latency phase instrumentation
  - ADR-0089: AuthZ policy candidate index
  - `crates/temper-authz/src/engine/candidates.rs`
  - `crates/temper-authz/src/engine/mod.rs`

## Context

ADR-0089 made Cedar authorization safer to optimize by filtering each request's `PolicySet` to only policies whose static principal/action/resource constraints could still match the request. That rollout is live in TemperPaw production as `7726e4dfba5453c337dafb7411f88c239f3d5bd5`.

The production proof on 2026-05-16 shows the first filter is semantically useful but not fast enough:

- `temper_cedar_policy_candidate_count` reduced the sampled policy volume from 16,497 full policies to 48 candidate policies per evaluation.
- `temper_cedar_evaluation_phase_duration_ms{phase:authorizer}` dropped to about 0.23 ms p95 in the corrected proof bucket.
- `temper_cedar_evaluation_phase_duration_ms{phase:policy_candidates}` remained about 13.4 ms p95, and sampled AuthZ spans still spent 19.6-26.1 ms around full authorization calls.

The cause is visible in the implementation: every authorization call scans `policy_set.policies()`, checks constraints one policy at a time, clones the matching policies, and builds a fresh `PolicySet`. That preserves correctness, but it keeps per-request CPU proportional to total policy count. With thousands of generated policies, that is exactly the wrong hot-path shape.

## Decision

Precompute a per-policy-set candidate selection index at policy load time, then use it for request-time selection.

### Sub-Decision 1: Index Static Constraint Buckets

For each static Cedar policy, index the policy ordinal into principal, action, and resource buckets:

- Principal buckets: always-match, principal type, exact principal UID.
- Action buckets: always-match, exact action UID.
- Resource buckets: always-match, resource type, exact resource UID.

Request-time selection computes the union of matching buckets for each dimension, intersects the three sorted ordinal lists, and builds the candidate `PolicySet` from that small result.

**Why this approach**: ADR-0089 already defined the conservative scope rules. Indexing those rules avoids total policy scans while preserving the same "drop only impossible static mismatches" semantics.

### Sub-Decision 2: Keep Decision Caching Out Of Scope

This change does not cache allow/deny decisions. It may reuse the static candidate index for a policy set, but every request still builds Cedar entities/context and calls Cedar's authorizer.

**Why this approach**: Authorization decisions depend on context, principal attributes, resource attributes, forbids, and future policy changes. Decision caching needs a separate invalidation design. The current bottleneck is candidate selection, so the smaller fix is safer.

### Sub-Decision 3: Fail Closed To Full Policy Set

If a policy is non-static, template-linked, or cannot be indexed safely, the index marks the policy set as fallback. Runtime selection then uses the full policy set and records fallback candidate counts.

**Why this approach**: A missed forbid or missed broad permit would be worse than a slow request. Fallback preserves Cedar's original semantics when the optimizer cannot prove a policy's static scope.

### Sub-Decision 4: Rebuild On Policy Reload

Tenant policy reloads and fallback policy reloads rebuild the index atomically with the `PolicySet`.

**Why this approach**: Policy reload is already the trust boundary for replacing compiled Cedar policy text. Keeping the index inside the compiled policy bundle gives simple invalidation with no cross-request cache lifecycle.

## Rollout Plan

1. **Phase 0 (Immediate)** - Add the indexed selector and wire `AuthzEngine` to store a compiled policy bundle containing both the `PolicySet` and the candidate index.
2. **Phase 1 (Verification)** - Add semantic equivalence tests for permits, forbids, empty candidate sets, exact UID constraints, broad hierarchy constraints, tenant reloads, and named policy diagnostics.
3. **Phase 2 (Production Proof)** - Roll through Temper and TemperPaw, then rerun the File/OData proof and verify that `phase:policy_candidates` p95 falls below 2 ms while candidate counts and authorization decisions remain correct.

## Readiness Gates

- `cargo test -p temper-authz` passes.
- Focused tests prove indexed and scan-based selection produce equivalent Cedar decisions and diagnostics.
- `cargo check -p temper-server` passes because Temper server depends on the changed authz engine surface.
- Datadog shows live `temper_cedar_policy_candidate_count` and reduced `phase:policy_candidates` after deployment.
- Live OData read-after-write proof still reaches `Ready` and returns matching bytes.

## Consequences

### Positive

- Request-time candidate selection becomes proportional to matching bucket sizes instead of total policy count.
- Cedar evaluator work remains small without relying on authorization decision caching.
- Tenant policy reload naturally invalidates the index.

### Negative

- `temper-authz` owns more in-memory policy metadata.
- Candidate selection code becomes more complex than a linear scan.
- Broad policies still appear in every request's bucket; those must be optimized by policy design or a later semantic index.

### Risks

- A bucket classification bug could omit a policy. Mitigation: keep rules identical to ADR-0089, test forbids and exact UID policies, and fallback to full policy set on unsupported shapes.
- `PolicySet::from_policies` may still be measurable if candidate sets are large. Mitigation: measure after index rollout before adding a reusable candidate `PolicySet` cache.

### DST Compliance

This change is confined to `temper-authz`, not the simulation-visible crates listed in the project guide. The implementation still prefers deterministic `BTreeMap` ordering for stable tests and reproducible behavior.

## Non-Goals

- No allow/deny decision cache.
- No Cedar bypass.
- No changes to Cedar policy language or generated app policy shape.
- No weakening of default deny, forbid override, tenant isolation, or named policy diagnostics.

## Alternatives Considered

1. **Keep the ADR-0089 linear scan** - Rejected because production Datadog shows the candidate-selection phase is now the AuthZ bottleneck.
2. **Cache final authorization decisions** - Rejected for this slice because invalidation across context/resource/principal/policy changes is a separate correctness problem.
3. **Prebuild every possible candidate `PolicySet`** - Rejected because exact principal/resource UID combinations can be high-cardinality and unbounded.
4. **Skip broad hierarchy policies aggressively** - Rejected because Cedar entity hierarchy semantics need explicit entity data; conservative inclusion is safer.

## Rollback Policy

If the index causes incorrect decisions or measurable regressions, revert the engine to ADR-0089's linear `select_candidate_policy_set` call. The metrics and tests added for this change remain useful because they identify the candidate-selection phase independently from Cedar authorizer time.
