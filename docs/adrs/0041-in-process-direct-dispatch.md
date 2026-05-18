# ADR-0041: In-process direct-dispatch for kernel-resident callers

- Status: Proposed
- Date: 2026-05-18
- Deciders: Temper core maintainers
- Related:
  - ADR-0002: WASM integration for agent-generated API calls
  - ADR-0040: Composite-action kernel primitive (this PR)
  - ADR-0039: Latency observability acceleration program
  - `nerdsane/temper-git` RFC-0003: Genesis app registry
  - `crates/temper-runtime`, `crates/temper-server`, `crates/wasm-host`

## Context

Today, every Temper action invocation flows through the same pipeline,
regardless of caller location:

```
caller → host_http_call → HTTP router → path resolution →
         OData parser → action dispatcher → action handler
```

This is correct and necessary for external callers — a laptop running
`curl`, another box on the network, an MCP client over HTTP, all need
the HTTP and OData layers because that's the wire format.

For **internal callers** (a WASM module running inside the same
kernel, an OS app invoking another action, the kernel's own
projections), the HTTP and OData layers are pure overhead:

- The caller already has typed arguments. There's no JSON to parse.
- The caller knows the target action by identity, not by URL path.
  There's nothing for the router to resolve.
- The caller is in the same process; there's no network round-trip
  to amortize the layers against.

Today, an internal caller pays the same per-call cost as a remote
caller: ~100µs–1ms per invocation depending on entity, plus the
serialization tax. For the agent-resident hot path described in the
transmission log (regime A), this is the dominant cost — not the
actual work, but the wrapping around the work.

### Why this matters now

`nerdsane/temper-git` RFC-0003 (Genesis app registry) introduces
agent-resident workflows where many in-kernel calls happen per
operation:

- Forking an app does ~4 sub-writes via `Apps.Fork` composite action.
  Each sub-write inside the composite still pays the OData tax today.
- Installing an app fetches bytes via `git clone` (external) then
  performs ~5 in-kernel writes (App row, Lineage row, Closure
  computation). Each pays the OData tax.
- The agent's hot loop inside an operator's TemperPaw makes many
  reads and writes per work-cycle iteration. Each pays the OData
  tax.

With direct-dispatch, these internal calls cost ~10µs each instead of
~100µs–1ms. That's the difference between "fast" and the regime-A
"exceptional" the transmission log Q5 identifies — the kernel
primitive that, combined with composite actions (ADR-0040), lets a
fully-governed mutation complete in ~100µs end-to-end.

### Where this fits in the µs-floor primitive set

The transmission log Q5 lists six kernel primitives that, together,
get Temper to the microsecond-scale latency floor:

1. **In-process direct-dispatch (this ADR)**
2. **Composite actions** (ADR-0040)
3. Pre-compiled Cedar (future)
4. Hot in-memory projections (future)
5. Group-commit on the event log (future)
6. Reactive subscriptions inside the log (future)

This ADR and ADR-0040 are the two v1 items. The other four are 10–50×
speedups on specific dimensions but not v1 blockers for the registry
correctness — they're future optimization work.

## Decision

**Add `host_call(action_id, typed_args)` as a kernel-internal host
function that skips HTTP and OData for in-kernel callers. Cedar,
spec validation, state machine transitions, and event log appends are
preserved exactly.**

### Sub-decision 1: New host function

WASM modules and integration code gain access to:

```rust
// pseudo-API
fn host_call<A: Action>(
    action_id: ActionId,
    args: A::Args,
    ctx: &CallContext,
) -> Result<A::Output>
```

Where:

- `action_id` identifies the target action (entity + action name,
  resolved at install time to an index).
- `args` is the typed argument struct for the action.
- `ctx` carries the principal and tenant context from the calling
  invocation.

Internally, `host_call`:

1. Looks up the action by index (O(1) array dereference, no path
   resolution).
2. Runs Cedar evaluation using the caller's principal and the target
   resource.
3. Runs spec validation on the typed args (same as the OData path).
4. Dispatches directly to the action handler.
5. Appends event to the log.
6. Updates projections.
7. Returns typed output.

Compared to the HTTP path, steps that are skipped:

- HTTP framing / TLS (already not present for in-process WASM, but
  not eliminated cleanly today).
- HTTP router path matching.
- OData URL parsing.
- JSON serialization of args.
- JSON deserialization of args.

Compared to the HTTP path, steps that are preserved:

- Cedar evaluation. **Authorization is not bypassed.**
- Spec validation. Field types, formulas, refs still checked.
- State machine transitions. Invariants still enforced.
- Event log append. Durability preserved.
- Projection updates. Read-after-write consistency preserved.

### Sub-decision 2: External callers continue to use the HTTP path

This ADR adds a new path; it does not remove the existing one.
External callers (network clients, MCP-over-HTTP, browsers) keep using
OData over HTTP exactly as they do today.

### Sub-decision 3: Action identity is stable across deployments

`ActionId` is derived from the (app, entity, action_name) triple at
spec-install time. The mapping is stable: the same logical action has
the same ID across deployments, replays, and snapshots.

This matters for:

- Replay correctness (events reference action_id; replay must
  resolve them identically).
- Cross-app calls (an OS app can call another OS app's action by ID
  without going through OData).
- Future federation (a federated kernel can route an action by ID
  without re-parsing).

### Sub-decision 4: Cedar context carries through

The calling invocation's principal, scope, and tenant attribution
propagate to the called action. This is critical: an agent acting on
behalf of principal P, calling action X via `host_call`, must have X
evaluated under P's authority, not the kernel's.

`CallContext` carries:

- `principal`: the original caller's identity
- `tenant`: tenant scope
- `parent_event_id`: the event that initiated this chain (for
  audit-trail attribution)

Cedar policies see the same principal regardless of whether the call
arrived via HTTP or `host_call`. Behavior is identical from a
policy perspective.

### Sub-decision 5: Bounded mailbox semantics still apply

Each action has a bounded mailbox (TigerStyle). `host_call` respects
these limits the same way HTTP-path calls do. If the mailbox is full,
`host_call` returns an error; it does not bypass backpressure.

## Rollout Plan

1. **Phase 0 (this PR).** Spec'd here; no code yet. Defines the
   contract.
2. **Phase 1.** Kernel implementation:
   - `host_call` host function in `temper-runtime`
   - Action ID resolution at spec-install time in `temper-jit`
   - Cedar context propagation through `host_call`
   - WASM host binding for `host_call`
3. **Phase 2.** Adoption in temper-git:
   - `Apps.Fork` composite action's sub-writes use `host_call`
   - `Repository.WriteFile`'s internal sub-writes migrate
4. **Phase 3.** Adoption in paw-* apps:
   - paw-heal, paw-harness internal cross-action calls migrate
5. **Phase 4.** Latency observability harness verifies microsecond-
   scale internal calls.

## Readiness Gates

- Microbenchmark: `host_call` to a no-op action completes in ≤ 20µs.
- End-to-end: `Apps.Fork` composite (with ~4 sub-writes via
  `host_call`) completes in ≤ 100µs.
- All existing apps that adopt `host_call` produce identical event
  logs to their HTTP-path baseline (regression coverage).
- Cedar policies see the same principal in `host_call` as in HTTP
  path (security regression test).
- DST-mode tests pass: action ID resolution is deterministic across
  replays.

## Consequences

### Positive

- **~10× latency reduction** for in-kernel calls. Per the
  transmission log Q5 analysis, this is the largest single source of
  µs-floor improvement available.
- **Composite-action sub-writes get full benefit.** When a composite
  action's sub-writes are issued via `host_call`, each costs ~10µs
  instead of ~100µs–1ms. A composite with 50 sub-writes goes from
  ~5ms to ~500µs (excluding the actual work).
- **Audit trail unchanged.** Events, principals, and Cedar decisions
  flow through unchanged; only the parsing/dispatch overhead is
  eliminated.
- **Generalizes.** Any kernel-resident WASM module or integration
  benefits, not just the registry.

### Negative

- **Two dispatch paths to maintain.** External HTTP and internal
  `host_call`. Mitigated by sharing the same downstream pipeline
  (Cedar, spec validation, state machine, log append) — only the
  entry/exit layers differ.
- **Action ID stability is a new contract.** Spec changes that
  rename/remove actions must preserve IDs or be flagged as breaking.

### Risks

- **Cedar context propagation bug.** If a `host_call` invocation
  fails to propagate principal correctly, an action runs under the
  wrong authority. Mitigated by regression tests that assert
  principal identity across `host_call` chains.
- **Action ID drift.** If install-time ID resolution is non-
  deterministic, replay breaks. Mitigated by deriving IDs from
  canonical spec content (hash-based); DST tests verify stability.

### DST Compliance

- Action ID resolution is purely functional given canonical spec
  inputs. Deterministic across replays.
- `host_call` execution order within a tenant is deterministic
  (single-threaded actor model preserved).
- `CallContext.parent_event_id` uses `sim_uuid()` in simulation-
  visible code paths.
- No `// determinism-ok` annotations expected.

## Non-Goals

- Eliminating Cedar evaluation (we explicitly keep Cedar in `host_call`
  — that's not what this ADR optimizes).
- Pre-compiled Cedar (separate µs-floor primitive; future).
- Hot in-memory projections (separate; future).
- Cross-kernel direct-dispatch (federation work; future).

## Alternatives Considered

### Caching the HTTP-router lookup (rejected)

Keep HTTP-path dispatch but cache router decisions for fast paths.

**Rejected because:** doesn't eliminate JSON ser/deser; doesn't
eliminate path-parsing entirely; saves at most ~30% of the overhead.
Direct-dispatch eliminates the entire layer.

### Skip Cedar for in-kernel callers (firmly rejected)

Argue that in-kernel callers are trusted and Cedar is unnecessary.

**Rejected because:** authorization is the entire point of Temper's
substrate. Skipping Cedar for in-kernel callers introduces a
backchannel for unauthorized writes; the agent's hot path being
fast is meaningless if it's not also governed. Cedar stays.

### A `host_batch_insert` for bulk writes only (rejected)

Add a narrow optimization for bulk inserts, not a general
direct-dispatch primitive.

**Rejected because:** misses the general win. Many in-kernel calls
aren't bulk inserts; they're individual reads, individual updates,
cross-action calls. A general primitive serves them all.

## Rollback Policy

If direct-dispatch proves wrong:

1. Revert the `host_call` host function.
2. WASM modules and integration code that adopted it fall back to
   `host_http_call`.
3. Performance regresses to the HTTP-path baseline; correctness
   unchanged.

Rollback is low-risk because direct-dispatch is additive. The HTTP
path remains the default for external callers and the fallback for
internal callers.
