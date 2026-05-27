# ADR-0122: Genesis Pinned App Install

- Status: Accepted
- Date: 2026-05-27
- Deciders: Temper core maintainers
- Related:
  - ADR-0043: Git App Sources
  - ADR-0062: Delta OS App Reconcile And WASM Artifacts
  - ADR-0120: Directed Evolution Control Plane
  - `crates/temper-platform/src/genesis_install.rs`

## Context

Directed Evolution creates variant app commits on branch refs and installs each
variant into an isolated tenant so AI simulated users can exercise the live
OData surface. The public install vocabulary already accepts pinned Genesis refs
such as `owner/app@hash`, but the server-side `App.Install` hook materialized the
App row's `LatestVersionHash` instead of the pinned `AppRef` supplied in the
action params.

That made variant runtime refs look correct while the tenant still served the
parent app's metadata. Evaluators correctly failed those variants because
actions and fields added by the variant commit were not reachable through live
OData.

## Decision

`App.Install` must honor the pinned `AppRef` passed by the governed action. When
`AppRef` is present, the hook validates that its owner and name match the App row
and materializes the requested hash. When `AppRef` is absent or unpinned, the
hook keeps the existing behavior of installing the App row's
`LatestVersionHash`.

The Genesis bundle export endpoint also treats the path hash as the requested
version for the matching App row rather than requiring it to equal
`LatestVersionHash`. The Git object store remains the source of truth: a missing
commit/tree/blob still fails materialization.

## Rollout Plan

1. **Phase 0 (Immediate)** — Fix the install hook and bundle resolver; add unit
   coverage for pinned install ref resolution.
2. **Phase 1 (Directed Evolution proof)** — Deploy the platform once, rerun the
   live Agent Answers episode, and verify variant tenants expose their pinned
   CSDL/action changes.
3. **Phase 2 (Hardening)** — Add an end-to-end Genesis install smoke that pushes
   a non-latest branch commit and installs it by pinned ref.

## Readiness Gates

- Installing `owner/app@variant_hash` exposes the variant CSDL in the target
  tenant without advancing the registry App row's latest hash.
- Directed Evolution evaluators can exercise the variant tenant through live
  OData and include runtime evidence.

## Consequences

### Positive

- Directed Evolution can evaluate multiple branch commits in parallel without
  mutating the registry App row for each variant.
- The user-facing install contract matches the documented pinned-ref semantics.

### Negative

- Bundle export can now expose any commit that belongs to the app repository, so
  callers must continue to provide exact pinned refs.

### Risks

- A caller may request an old commit intentionally or accidentally. This is
  mitigated by explicit hash pinning and by preserving missing object failures.

### DST Compliance

- This change is in `temper-platform`, outside the simulation-visible runtime
  crates. No determinism annotations are required.

## Non-Goals

- This ADR does not add branch-name install refs.
- This ADR does not change how dependencies are resolved beyond preserving their
  existing pinned/latest behavior.

## Alternatives Considered

1. **Advance App.LatestVersionHash for every variant** — Rejected because it
   pollutes the registry row and prevents parallel variant installs from being
   independent.
2. **Use a worker-only Git clone path** — Rejected as the primary path because
   it bypasses the spec-owned `App.Install` surface the platform already exposes.

## Rollback Policy

Revert the hook and resolver changes. Existing latest-hash installs continue to
work, but Directed Evolution branch variants will again fail live hot-load
verification unless the registry App row is advanced before each install.
