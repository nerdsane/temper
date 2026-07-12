# ADR-0164: ActorContext namespace capability (ARN-215)

- Status: Accepted
- Date: 2026-07-12
- Deciders: Temper core maintainers
- Related:
  - ARN-215: ActorContext grants arbitrary cross-namespace persistence
  - `crates/temper-actor-runtime/src/actor.rs`

## Context

`load_actor_state` / `upsert_actor_state` accepted any namespace string, so a
compromised or buggy handler could read/overwrite arbitrary actor rows outside
its activation namespace.

## Decision

### Sub-Decision 1: Default bind to self namespace

Access to `namespace == self_handle.namespace` is always allowed.

### Sub-Decision 2: Explicit grant for cross-namespace (same tenant root)

Any other namespace requires a prior `grant_cross_namespace` on the context.
Grants are **restricted to the caller's tenant root** (first `/`-separated
segment of `self_handle.namespace`). A handler cannot self-grant into another
tenant's namespace tree. Path traversal, empty, absolute, and backslash
namespaces are rejected at grant time. Callers still validate untrusted path
segments (e.g. `process_id`) before composing the namespace string.

### Sub-Decision 3: Fail closed

Missing grant → `ActorError::NamespaceDenied`. Invalid grant targets also
return `NamespaceDenied` (no silent accept).

## Consequences

### Positive

- Cross-namespace persistence is capability-gated.
- Same-namespace integrations keep working unchanged.

### Negative

- Callers that intentionally touch sibling namespaces (e.g. child Process) must
  grant those namespaces before load/upsert.

## Non-Goals

- Full Cedar policy evaluation for each row (follow-up).
- Mailbox retention redesign (separate).
