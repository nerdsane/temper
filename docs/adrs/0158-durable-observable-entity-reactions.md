# ADR-0158: Durable and observable cross-entity reactions

- Status: Proposed
- Date: 2026-08-04
- Deciders: Temper core maintainers
- Supersedes: ADR-0092 (its acceptance of crash-loss for background reactions)
- Related:
  - ADR-0046: Unified action triggers and inherited Cedar authority
  - ADR-0139: Action bridge awaits reactions
  - ADR-0150: Composite verification and intentional `drop_ok`
  - ADR-0152: Integration failures are never silent
  - GitHub issue `nerdsane/temper#414`
  - `crates/temper-server/src/trigger/dispatcher.rs`
  - `crates/temper-server/src/state/dispatch/actions.rs`

## Context

Temper currently commits a source entity transition and only then schedules its
cross-entity reactions. Background delivery uses an in-memory task. A process
crash after the source commit but before the task runs permanently loses the
reaction. Awaited dispatch closes the response-ordering gap but not the crash
window, and logs do not provide a durable operator-visible outcome.

This violates the meaning of a required reaction. It also prevents applications
from safely relying on entity transitions alone for workflows such as automatic
construction of related entities and rules. Retrying the source action is not a
sound recovery mechanism: the action may no longer be enabled, and repeating it
may duplicate unrelated source effects.

The fix belongs in the generic kernel. Application-specific orchestration or an
operator override would conceal the same durability gap behind another caller.

## Decision

### 1. Persist a normalized delivery intent with the source event

For every entity trigger selected by a successful source transition, the kernel
normalizes the target, parameters, trigger policy, and invoking
`SecurityContext` into a reaction-delivery intent. The event store commits the
source event, derived projections, and all mandatory intents in one atomic
operation. If a backend cannot persist mandatory intents, the source commit
fails closed. `drop_ok` affects terminal delivery classification, not whether an
intent is durably recorded.

The stable delivery identifier is derived from tenant, source entity type and
ID, source action, committed source sequence, trigger identity, and trigger
index. It contains no secrets and is identical after replay or restart.

The current `[[action.triggers]]` syntax remains unchanged. Normalization is a
runtime storage concern, not a new application authoring primitive.

**Why this approach**: only an atomic source-event/outbox write eliminates the
crash gap. A scan of source events after restart cannot reconstruct registry
versions, resolved targets, parameters, or original authority with equal
fidelity.

### 2. Use a bounded, leased delivery lifecycle

Persisted delivery state is one of:

`Pending`, `Claimed`, `Dispatching`, `Succeeded`, `Skipped`, `DroppedAllowed`,
`Rejected`, or `DeadLettered`.

A worker claims at most ten deliveries concurrently. Claims carry a lease and a
monotonic fencing token. Expired claims return to the pending pool. Automatic
delivery receives at most five attempts with deterministic capped backoff.
Permanent authorization, target-resolution, malformed-input, and depth errors
do not spin; they reach the appropriate terminal state. Exhausted transient
errors become `DeadLettered`.

The maximum trigger depth remains eight. Depth and causation lineage are stored
on each intent, so restart cannot reset the budget.

Recovery discovers intents by keyset-paging durable source journals and reading
events under an explicit work budget. Lifecycle journals materialize the
inferred `Pending` state before a successful source response is returned, which
makes non-awaited work immediately observable without waiting for target
execution. Each tenant has one serialized recovery cursor, and a completed scan
retains the earliest logical retry or lease-expiry wakeup it observed. Redis
maintains its journal keyset as a sorted index in the same append script rather
than materializing the tenant's complete entity set in a worker.

A production recovery supervisor maintains an independent due time per tenant
and processes one bounded batch for each due tenant. Tenant-scoped notifications
wake newly durable non-awaited work, active keyset scans are briefly paced,
storage errors back off, and idle tenants poll slowly without inheriting another
tenant's scan cadence. A weak server-lifetime sentinel and generation fencing
retire workers without allowing the worker's own state clone or OData-only
bound-action hooks to retain the server indefinitely. Supervisor membership
comes from configured tenants rather than current reaction rules, because an
intent's persisted rule must remain deliverable after the live rule is removed.

When a delivered target event commits descendant intents, the delivering worker
materializes their lifecycle journals before marking the parent delivery
successful and wakes that tenant's recovery supervisor. Receipt reconciliation
repeats this materialization from the already committed target event, closing
the crash window between target commit and descendant lifecycle creation.

### 3. Couple the target effect to a durable receipt

Target dispatch uses the delivery ID as its idempotency identity. The target
event and a delivery receipt are committed atomically. Before retrying an
ambiguous `Dispatching` delivery, the worker reconciles the receipt. A matching
receipt marks the delivery `Succeeded`; absence permits another fenced attempt.
This prevents duplicate target effects across crashes at either side of the
target commit acknowledgement.

Receipt reconciliation reads the newest bounded target-event window. Its budget
equals the actor's durable idempotency-key retention budget, so the receipt and
the fallback idempotency proof have the same lifetime and neither path requires
an unbounded journal replay.

Component stores must implement the same semantic contract for Postgres, Turso,
Redis-backed deployments, and deterministic Sim storage. Redis may coordinate
work, but it cannot be the sole durable record of a cross-journal transaction.

### 4. Preserve the original authority

The normalized intent carries the original `SecurityContext` or the trigger's
explicit principal exactly as ADR-0046 defines. Every attempt repeats Cedar
authorization under that authority. Human operators may request a retry but do
not acquire or replace the delivery authority. There is no operator override.

Persisted authority is private security metadata. Observe responses expose only
redacted principal identity fields sufficient for audit and never tokens,
credentials, or arbitrary claims.

### 5. Define response and await semantics around durability

`await_reactions = false` returns only after the source event and its reaction
intents are durable. It does not wait for target execution.

`await_reactions = true` waits, within the caller's existing bounded deadline,
for the complete descendant delivery tree to reach terminal states. Descendants
are linked by a root delivery ID. Target failure never rolls back the already
committed source transition. The response reports incomplete or failed delivery
truthfully when the deadline or a terminal failure is reached. If bounded tree
inspection cannot prove that every descendant was examined, the await fails
conservatively instead of treating a truncated tenant listing as completion.

### 6. Make outcomes queryable and retries governed

The server adds:

- `GET /observe/reactions` for bounded, tenant-scoped delivery listing;
- `GET /observe/reactions/{delivery_id}` for redacted detail and attempt history;
- `POST /api/reactions/{delivery_id}/retry` for a governed retry request.

Manual retry is accepted only for transient `DeadLettered` deliveries, uses the
original authority, and is bounded to three requests. It creates another attempt
on the same delivery identity rather than a new logical reaction. Terminal
authorization rejections, malformed deliveries, depth exhaustion, successful
deliveries, and `DroppedAllowed` deliveries cannot be overridden.

Metrics cover queued, claimed, succeeded, terminal outcomes, retries, lease
recovery, reconciliation, latency, and queue age. Labels exclude entity IDs,
parameters, principal claims, and other high-cardinality or sensitive values.

## Rollout Plan

1. Add the storage contract, lifecycle model, and deterministic failing tests.
2. Implement atomic persistence and delivery for Sim and Turso, then Postgres and
   Redis deployment parity.
3. Switch inline and background trigger paths to durable intents and implement
   bounded descendant waiting.
4. Add Observe and governed retry surfaces, metrics, and redaction tests.
5. Exercise crash points before claim, during dispatch, after target commit, and
   before acknowledgement in deterministic simulation and a live local server.
6. Validate TemperPaw against an immutable development image pinned to the exact
   review commit before any downstream paid-provider experiment.

## Readiness Gates

- Source event and mandatory intents are proven atomic for every supported
  durable backend.
- Crash/restart tests prove eventual delivery without duplicate target effects.
- Missing targets, Cedar denial, conflicts, guard mismatch, depth exhaustion,
  `drop_ok`, and malformed intents have explicit durable outcomes.
- `await_reactions` tests cover successful, failed, timed-out, and multi-level
  descendant trees.
- Observe and retry APIs are tenant-isolated, redacted, bounded, and Cedar
  governed.
- DST review, code review, full workspace tests, and live E2E are clean.

## Consequences

### Positive

- A committed source transition can no longer silently lose a required reaction
  because the server restarted.
- Applications can model automatic entity construction as auditable entity
  transitions without external orchestration loops.
- Operators gain deterministic identities and durable outcomes for diagnosis and
  narrowly governed recovery.

### Negative

- Every source action with entity triggers adds durable write amplification.
- Stores gain bounded journal paging and recent-event-read contracts plus
  retention responsibilities.
- Awaited calls may observe a terminal target failure after the source transition
  is already committed; clients must not interpret that as source rollback.

### Risks

- A flawed lease implementation could permit concurrent attempts. Monotonic
  fencing plus receipt reconciliation is required before enabling workers.
- Delivery records contain security context. Private storage, encryption at rest,
  tenant isolation, and strict redaction are mandatory.
- Unbounded reaction graphs could grow storage and wait sets. Existing depth
  eight, concurrency ten, bounded attempts, deadlines, and retention controls
  constrain the work.

### DST Compliance

- Delivery IDs, claim ordering, backoff, lease time, and attempt selection use
  simulation-provided clocks/IDs and ordered collections.
- The Sim store models every crash boundary and backend transition; production
  workers do not introduce unbounded `tokio::spawn` fanout.
- No wall clock, OS randomness, filesystem, network, or environment access is
  added to simulation-visible logic.

## Non-Goals

- Changing trigger authoring syntax or adding application-specific reactions.
- Rolling back source transitions when descendants fail.
- Granting operators authority to bypass Cedar or alter a persisted principal.
- Treating external webhooks or arbitrary WASM side effects as exactly once;
  this ADR guarantees the kernel's cross-entity event and receipt boundary.
- Replacing Genesis release provenance or changing production release policy.

## Alternatives Considered

1. **Reconstruct reactions by scanning source events** — rejected because the
   exact resolved intent and original registry/security context may no longer be
   available.
2. **Keep in-memory dispatch and add more retries** — rejected because retries do
   not survive the source-commit crash window.
3. **Require callers to use `await_reactions = true`** — rejected because waiting
   does not make the source-to-intent boundary atomic and harms latency.
4. **Implement an ARC-specific repair script** — rejected because it bypasses
   the entity-state audit model and leaves every other application exposed.
5. **Use Redis alone as the outbox** — rejected because a second, non-atomic
   durable system recreates the split-brain window.

## Rollback Policy

Before enabling durable workers by default, rollback may remove the new storage
and routing path together. After enablement, rollback must first drain or export
all non-terminal deliveries and disable new intent creation; reverting only the
worker or only the atomic write would strand durable work or restore silent loss.
