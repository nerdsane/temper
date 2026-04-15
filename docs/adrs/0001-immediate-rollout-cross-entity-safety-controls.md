# ADR-0001: Enforce-Only Rollout for Cross-Entity Capabilities (Pre-Production)

- Status: Accepted
- Date: 2026-02-23
- Deciders: Temper core maintainers
- Related:
  - `README.md` (cross-entity invariants gap)
  - `docs/GAP_TRACKER.md` (cross-entity coordination resolved via reactions)
  - `crates/temper-server/src/reaction/dispatcher.rs` (current fire-and-forget behavior)

## Context

Temper currently supports cross-entity choreography through reaction rules, but it does not provide first-class cross-entity invariant enforcement. We are closing this gap and need to start rollout immediately.

The system is not yet in production. Because there is no live tenant traffic to protect, we do not need runtime dual-mode behavior (`shadow` + `enforce`) at this stage.

We still need safety controls during bring-up:

- Bad rules can block valid writes.
- Cross-entity checks add hot-path latency.
- Multi-entity workflows can fail in ways not visible in single-entity tests.

## Decision

We will use **enforce-only runtime behavior** during pre-production.

1. Ship relation integrity and cross-entity invariant checks now.
2. Run checks in `enforce` mode in all pre-production environments.
3. Keep a single emergency bypass switch to temporarily disable enforcement if bring-up is blocked.

`enforce` behavior:

- Reject writes that violate `hard` invariants.
- Track `eventual` invariants with bounded convergence windows and alert when unresolved.
- Emit violations, traces, counters, and rejection reasons for debugging.

## Rollout Plan

1. **Phase 0 (Immediate, same release)**  
   Implement enforcement checks, rejection paths, observability, and an emergency bypass.

2. **Phase 1 (Integration + DST simulation)**  
   Run deterministic multi-entity simulations (including reaction cascades and fault injection) and fix all invariant violations or false positives.

3. **Phase 2 (Pre-production soak)**  
   Run sustained load in staging with enforcement enabled; validate latency/error budgets and convergence behavior for eventual invariants.

4. **Phase 3 (Production readiness gate)**  
   Do not go live until readiness gates pass for a defined window.

5. **Phase 4 (Production launch)**  
   Launch with enforcement enabled by default.

## Readiness Gates

Production launch is blocked unless all are true for 7 consecutive days in pre-production soak:

- Reaction dispatch failure rate `< 0.1%`
- Cross-entity check execution error rate `< 0.05%`
- Unresolved eventual invariant rate `< 0.05%`
- p95 convergence latency `< 5s` and p99 `< 30s`
- p95 write-path latency regression from enforcement `< 10%`
- No Sev-1/Sev-2 incidents attributable to enforcement logic

## Rollback Policy

- Emergency bypass: disable cross-entity enforcement globally via runtime config.
- Scope reduction: disable only eventual-invariant rejection while keeping hard invariants enabled (if supported by implementation flags).
- No rollback requires schema reversal; controls are runtime-configurable.

## Consequences

Positive:

- Simpler runtime model than dual-mode rollout.
- Finds correctness issues early while system is still pre-production.
- Avoids carrying temporary shadow-only code into production.

Negative:

- Higher risk of blocked workflows during early bring-up.
- Requires stronger test and soak discipline before launch.
- Emergency bypass must be tightly controlled to avoid masking real defects.

## Non-Goals

- Enforcing all invariants globally on day one.
- Introducing distributed transactions across entities.
- Coupling cross-entity checks to speculative, unverified business logic.
