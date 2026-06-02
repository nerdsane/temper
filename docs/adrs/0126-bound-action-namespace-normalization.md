# ADR-0126: Bound Action Namespace Normalization

- Status: Accepted
- Date: 2026-06-01
- Deciders: Temper core maintainers
- Related:
  - ADR-0125: Canonical Genesis Apps And Paw Orchestration
  - `crates/temper-server/src/odata/write.rs`
  - `crates/temper-server/src/odata/bindings.rs`

## Context

OData clients may call bound actions using namespace-qualified action names such
as `Temper.PawOrchestration.ReportHeartbeat`. The Postgres-backed actor path
already strips that namespace before dispatching the spec action, but the
non-PG bound-action path forwarded the fully-qualified action to Cedar,
prechecks, and the transition dispatcher.

This made installed app policies and specs disagree with the request boundary:
app bundles define actions like `ReportHeartbeat`, while the router could ask
Cedar and the transition table about `Temper.PawOrchestration.ReportHeartbeat`.
The mismatch blocks Genesis-installed app actions, including worker lifecycle
actions required by Directed Evolution.

## Decision

Normalize bound-action names once at the non-PG OData boundary:

- keep the raw OData action in trace attributes for diagnostics;
- use the unqualified action name for Cedar authorization;
- use the unqualified action name for write prechecks;
- use the unqualified action name for transition dispatch;
- pass the unqualified action name to post-action hooks.

This aligns the non-PG path with the existing PG actor path and keeps app specs,
Cedar policies, and CSDL namespace-qualified URLs interoperable.

## Rollout Plan

1. **Phase 0 (Immediate)** — Normalize non-PG bound actions and add tests for a
   namespaced OData call.
2. **Phase 1** — Deploy the backend and re-run the `temperpaw/paw-orchestration`
   worker heartbeat/claim proof against a fresh tenant.

## Readiness Gates

- A namespace-qualified OData bound action authorizes against an unqualified
  Cedar action name.
- The same call dispatches the unqualified transition name.
- Existing post-action hooks continue to work.

## Consequences

### Positive

- CSDL namespace-qualified clients no longer need duplicate Cedar action names.
- PG and non-PG action dispatch semantics match.
- Genesis-installed `temperpaw/paw-orchestration` worker actions can use normal
  action names in their specs and policies.

### Negative

- Trace consumers must use `odata.action.raw` if they need the client-supplied
  qualified name.

### Risks

- A tenant could theoretically define two actions that differ only by namespace.
  Temper specs do not model bound actions that way today, so the unqualified name
  is the canonical transition identity.

### DST Compliance

- This touches `temper-server`, a simulation-visible crate.
- The change is pure string normalization at the request boundary. It introduces
  no wall-clock time, random data, unordered iteration, threading, filesystem, or
  network behavior.

## Non-Goals

- Changing CSDL action naming.
- Requiring app policies to enumerate namespace-qualified aliases.
- Changing PG-backed actor dispatch.

## Alternatives Considered

1. **Duplicate namespaced actions in Cedar policies** — Rejected because it
   makes app bundles encode transport details and still leaves transition
   dispatch mismatched.
2. **Normalize inside the authz engine only** — Rejected because dispatch and
   prechecks also need the spec action name.

## Rollback Policy

Revert this ADR and the corresponding router change. A rollback would require
app bundles to add namespace-qualified policy/action compatibility shims.
