# ADR-0140: A Composite Action Fails When Its Integration Fails

## Status

Accepted

## Context

A Composite action's effect *is* its integration's `sub_writes`: the integration
runs, returns the sub-writes, and the kernel applies them (ADR-0139 ensures this
happens before the protocol response on the bridge path). The integration is the
whole point of the action — `Repository.IngestPack` has no effect except the
objects and refs its module produces; `Repository.MergePullRequest` has no effect
except the merge commit, the advanced ref, and the PR transition.

When that integration fails, `dispatch_wasm_integrations_internal` previously
routed every failure through `handle_wasm_failure`. With no `on_failure` handler
declared, `handle_wasm_failure` returns `Ok(None)` — which the dispatch loop
treats as *success*. So a Composite action whose integration failed reported
success to the caller while none of its declared sub-writes existed:

- A `Repository.IngestPack` whose module returned an error staged nothing but
  answered the push `200`; the client believed objects were stored that were not.
- A `Repository.MergePullRequest` whose `scm_merge_pr` reported a content
  conflict (`merge-conflict: ...`) answered the merge as if it had succeeded.

This is a silent-failure class — the same shape that hid the cold-boot push bug
(ADR-0139) — and it spans both failure modes: a clean `{success: false}` return
*and* a host trap / fuel exhaustion / panic inside the module.

## Decision

When a WASM integration fails for an action that is **Composite** and declares
**no `on_failure` handler**, the dispatch propagates the failure as `Err` instead
of absorbing it. The decision is made by `composite_failure_must_propagate`, which
is applied to both integration-failure arms (unsuccessful result and host error),
and **fails closed**: if compositeness cannot be determined (e.g. a poisoned
registry lock), it propagates rather than risk dropping sub-writes.

Actions that declare `on_failure` are unchanged — their recovery handler still
runs. Non-composite actions are unchanged.

## What this does and does not do

By the time integrations run, the action's parent state transition is already
durable (`run_post_dispatch_effects` runs after the actor commits the event).
Returning `Err` therefore **reports the action as failed to the caller** — it does
**not** roll the parent transition back. The protocol handler maps the propagated
error onto the right status (the genesis `github_rest_pulls` module maps
`merge-conflict:` onto HTTP `409`).

The corollary is a design constraint, not a bug: **a Composite action's parent
transition must be a safe no-op when its integration fails.** This is why the
genesis merge engine is driven by `Repository.MergePullRequest` (parent
`Active -> Active`) with the `PullRequest.Merge` transition carried as a
*conditional* sub-write — a conflict leaves the PR open and unmerged. An action
that moved its entity into a half-done state in the parent transition and relied
on the integration to finish would leave that state stranded on failure; such an
action must either make the parent transition idempotent/no-op or declare an
`on_failure` that reverts it.

## Consequences

- Composite integration failures (clean or trapping) surface to the caller
  instead of masquerading as success; the genesis conflict path returns `409`
  with the pull request untouched, verified by the live workflow smoke
  (`scripts/live-github-workflow-smoke.sh`, step "conflicting merge -> 409").
- App authors writing Composite actions on the bridge must keep the parent
  transition safe-on-failure (see above) or declare `on_failure`.
- DST-neutral: a metadata lookup plus a conditional `Err` return; no clock,
  randomness, threads, or I/O introduced.
