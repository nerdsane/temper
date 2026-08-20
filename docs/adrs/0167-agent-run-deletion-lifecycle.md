# ADR-0167: Agent-run deletion is a teardown-gated lifecycle

- Status: Accepted
- Date: 2026-08-20
- Deciders: Temper core maintainers
- Related:
  - ADR-0152: Integration failure never silent
  - `os-apps/temper-agent/specs/temper_agent.ioa.toml`
  - `crates/temper-server/src/agent_runtime/handlers.rs`

## Context

Agent runs provision per-run sandboxes. The existing `POST /v1/agent-runs/{id}/cancel` transition tears down a sandbox only while a run is active. Terminal runs (`Completed`, `Failed`, or `Cancelled`) cannot be cancelled, and the agent-runtime API has no deletion route. Generic OData entity deletion exists, but it does not run the `sandbox_destroyer` integration; using it would orphan Tensorlake or E2B compute.

A direct HTTP handler that deletes the sandbox and then erases the entity would either bypass the existing spec/Cedar lifecycle or create an irreversible race when the sandbox provider call fails after the run record has been removed.

## Decision

### Sub-Decision 1: Model deletion as state transitions

Add `Deleting`, `DeletionFailed`, and `Deleted` states to `TemperAgent`.

```text
Completed | Failed | Cancelled
  → RequestDeletion → Deleting
  → destroy_sandbox
  → DeletionTeardownSucceeded → Deleted

DeletionFailed → RetryDeletion → Deleting
```

`Deleted` is Temper's existing logical-deletion state. It is hidden by normal query/read paths but preserves the event-sourced audit trail.

**Why this approach**: The record stays visible while cleanup is outstanding. A provider teardown failure is explicit and retryable, rather than silently orphaning compute or deleting the evidence needed to retry.

### Sub-Decision 2: Add an idempotent agent-runtime DELETE route

`DELETE /v1/agent-runs/{id}` starts deletion for terminal runs, retries both `DeletionFailed` and interrupted `Deleting` teardown, and Cedar-authorizes every idempotent lifecycle response using the deletion capability. Active runs remain ineligible; callers must cancel them first.

**Why this approach**: The route gives clients a stable lifecycle API while Cedar continues to authorize the corresponding spec actions. It does not invoke provider APIs directly or duplicate secret-handling logic in the HTTP server.

### Sub-Decision 3: Persist external allocation before bootstrap can fail

Provisioning is split into two governed trigger stages:

```text
Provision → allocate_sandbox → SandboxAllocated (Provisioning)
SandboxAllocated → bootstrap_sandbox → SandboxReady (Thinking)
```

`SandboxAllocated` durably stores the actual selected provider, sandbox ID, and URL immediately after provider creation succeeds. TemperFS initialization, private-repository clone, and other fallible bootstrap work run only after that event has been persisted. Therefore, a bootstrap failure transitions the run to `Failed` while retaining the external-resource identity needed for a later teardown.

**Why this approach**: A provider sandbox can exist before clone or workspace initialization completes. Treating allocation as a durable boundary prevents a failed bootstrap from losing the only reference required to clean up provider compute.

### Sub-Decision 4: Make sandbox-destroyer outcomes explicit during deletion

For a `Deleting` agent, `sandbox_destroyer` dispatches `DeletionTeardownSucceeded` only after a successful provider teardown. A missing sandbox ID is successful only for an explicit `local` provider or `static-sandbox`; it is a `DeletionTeardownFailed` outcome for a remote provider. Provider `404 Not Found` is successful teardown because the provider resource is already absent. On failure it dispatches `DeletionTeardownFailed` with a bounded error message. Existing cancellation preserves its current best-effort teardown behavior.

**Why this approach**: Cancellation is already terminal and best-effort. Deletion must be stronger: it cannot claim that the run is gone while the provider resource remains.

## Rollout Plan

1. Add the spec states/actions and Cedar permits.
2. Split provisioning into durable allocation and fallible bootstrap triggers.
3. Add the HTTP delete route and response model.
4. Update the destroyer callbacks, build all WASM modules, and run the IOA/parser/JIT tests.
5. Restart the local server and validate both completed-run deletion and clone-failure deletion against Tensorlake.

## Consequences

### Positive

- Completed and failed runs can be cleaned up without orphaning sandboxes.
- Deletion is auditable, idempotent, and retryable.
- Provider teardown remains inside the existing WASM secret/governance boundary.

### Negative

- Deletion is asynchronous; callers must poll after a 202 response.
- A failed deletion remains visible as `DeletionFailed` until retried.

### Risks

- A provider may return a transient failure. The explicit failure state and retry endpoint behavior preserve enough state for a later retry.
- Existing cancellation teardown remains best-effort by design and is not retroactively changed.
- Allocation-stage persistence adds one extra governed transition before the first model call.

### DST Compliance

The changes are in `temper-server` and WASM integrations. They do not add clocks, randomness, threads, filesystem access, or network activity to simulation-visible runtime code; provider I/O remains in the production WASM host boundary.

## Non-Goals

- Physical erasure of agent event history.
- Deleting active runs without cancellation.
- Automatic deletion of checkpoints or external Git repositories.
- Changing existing cancellation semantics.

## Alternatives Considered

1. **Generic OData DELETE** — Rejected because it does not trigger sandbox teardown.
2. **Synchronous provider deletion in the HTTP handler** — Rejected because it duplicates provider and secret logic outside the governed WASM lifecycle and cannot preserve a retryable state after partial failure.
3. **Allow Cancel from terminal states** — Rejected because it conflates cancellation with deletion and preserves terminal run records without a deletion contract.

## Rollback Policy

Remove the agent-runtime DELETE route and stop exposing the new actions. Existing `Deleted` events remain logically deleted and auditable; no sandbox is recreated during rollback.
