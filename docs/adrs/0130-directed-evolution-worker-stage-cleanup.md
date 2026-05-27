# ADR-0130: Directed Evolution Worker Stage Cleanup

- Status: Accepted
- Date: 2026-05-27
- Deciders: Temper core maintainers
- Related:
  - ADR-0128: Directed Evolution Policy-Gated Repair Autostart
  - ADR-0129: Directed Evolution Repair-Aware Variant Lanes
  - `os-apps/directed-evolution/policies/directed_evolution.cedar`
  - `crates/paw-codex-worker/src/directed_evolution.rs`

## Context

Directed Evolution repair episodes now run real Codex worker evaluations against
hot-loaded variant tenants. When a simulated-user or reviewer stage eliminates a
variant, sibling stage work for that same variant can still be queued. The
TemperPaw worker detects those stale work items and tries to mark their
`StageResult` as eliminated before canceling the queued work item, so Mission
Control does not show permanently running stages for dead variants.

The app policy allowed `codex`, `system`, `supervisor`, and `human` principals to
move `StageResult`s, but not the `worker` principal. That was correct for
evaluation outcomes, yet too narrow for stale cleanup: the worker does not decide
fitness, it records that an already-terminal variant no longer needs sibling
stage work.

## Decision

Permit `worker` principals to invoke only `EliminateStageResult` on
`StageResult`.

The existing evaluator permit remains unchanged: `StartStageResult`,
`PassStageResult`, `FailStageResult`, and `EliminateStageResult` stay available
to `system`, `codex`, `supervisor`, and `human`. The new worker permit is
narrowly scoped to elimination cleanup and does not grant workers the ability to
pass or fail evaluations.

## Rollout Plan

1. **Phase 0 (Immediate)** - Ship the policy change in the Directed Evolution OS
   app and hot-load the updated app ref into active Directed Evolution tenants.
2. **Phase 1 (Validation)** - Restart the local TemperPaw worker and verify stale
   reviewer/simulated-user work is canceled after a variant is terminal.
3. **Phase 2 (UI)** - Surface stale-stage cleanup in Mission Control as a real
   elimination reason.

## Readiness Gates

- A live repair episode can continue after one variant is eliminated by a
  simulated-user stage.
- Queued sibling stage work for eliminated variants does not leave zombie
  `Running` stage results.
- The worker still cannot pass or fail a stage result directly.

## Consequences

### Positive

- Repair episodes keep moving after a candidate dies.
- Mission Control stage state remains truthful instead of showing stale running
  work.
- No platform deploy is needed; the fix ships through app hot-loading.

### Negative

- The policy trusts the worker to use `EliminateStageResult` only for stale
  cleanup. The worker code remains responsible for checking variant terminal
  state first.

### Risks

- A misconfigured worker could eliminate a stage prematurely. The mitigation is
  that the worker-side stale cleanup path only runs for reviewer/simulated-user
  work targeting a `StageResult` whose variant is already terminal.

### DST Compliance

- This is a Cedar policy change inside a Temper-native OS app. It does not touch
  simulation-visible Rust crates or introduce nondeterministic runtime behavior.

## Non-Goals

- Granting workers authority to pass or fail evaluations.
- Changing the evaluation or selection rules.
- Deploying a new Temper server image.

## Alternatives Considered

1. **Let stale stage results remain running** - Rejected because it makes live
   Mission Control state misleading and can block generation completion.
2. **Grant worker full StageResult authority** - Rejected because passing and
   failing should remain evaluator/operator decisions.
3. **Route cleanup through a Codex brain run** - Rejected because stale cleanup is
   deterministic bookkeeping after a terminal variant, not a new evaluation.

## Rollback Policy

Remove the worker `EliminateStageResult` permit and hot-load the prior Directed
Evolution app ref. Existing eliminated stage results remain historical evidence.
