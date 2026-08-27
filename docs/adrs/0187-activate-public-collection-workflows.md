# ADR-0187: Activate public collection workflows with ARC import proof

- Status: Accepted
- Date: 2026-08-26
- Deciders: Temper core maintainers
- Related:
  - ADR-0181: Verified bounded collection workflows
  - Fork issues #16 and #39
  - `crates/temper-jit/src/table/`
  - `crates/temper-server/src/trigger/collection_workflow/`
  - `crates/temper-wasm-sdk/`

## Context

ADR-0181 defined and verified bounded collection workflows. The parser,
verification cascade, durable ledger, delivery fencing, joins, recovery,
Observe API, and telemetry exist, but production compilation still rejects
every public declaration and the compiled transition table drops the verified
metadata. Consequently an installed application cannot exercise the contract.

Activation needs a reference application and a safe operational rollback. A
generic sample would prove only synthetic fan-out. ARC-AGI Temper's import is a
stronger bounded case: it seals an immutable roster, validates and materializes
each member, and reduces ordered evidence after each join without asking the
kernel to understand ARC tasks.

## Decision

### ARC-AGI Temper is the reference application

ARC-AGI Temper v0.12 is the sole activation proof. Cross-repository evidence
must be sanitized and include workflow/member transitions, Observe pages,
metrics, traces, restart identity stability, failure and control scenarios, and
the authentic 1,120-task import. No generic reference application is added.

ARC may retain final training, evaluation, and task counts plus ordered digests
as post-join domain evidence. It may not retain workflow cursors, outcome
counters, retry loops, or timers. All ARC validation and materialization
semantics remain in the application.

### Verified declarations become runtime metadata

The JIT carries verified `collection_workflow` declarations into transition
tables and registries. Normal source commits recognize each declaration's
start, cancel, and timeout actions; atomically create the workflow/control
ledger records; and hand bounded admission, member/cancel delivery, joins, and
recovery to the existing collection runtime. Installed declarations may reload
during draining because recovery is not new authoring.

Collection member, cancellation, and join intents declare the exact
`service:wasm-runtime` principal instead of inheriting the initiating operator.
Member delivery awaits its bound WASM integration; a committed member `Start`
receipt is admission evidence, not completion evidence. Inline WASM callbacks
derive a distinct stable idempotency key from the parent delivery, integration,
module, and callback action so a callback cannot reuse the cached `Start`
response or collide with another integration's callback.

If collection control races recovery, the original receipted delivery closes
as a workflow no-op and the fenced cancellation owns member terminalization.
Exact durable completion evidence and timeout-aware lease renewal for an
interrupted post-`Start` member remain activation prerequisites.

### Activation has three startup-validated modes

`TEMPER_COLLECTION_WORKFLOW_MODE` accepts exactly `enabled`, `draining`, or
`disabled` and defaults to `disabled`. Explicit activation remains gated on the
recovery prerequisites above. Any other value fails startup.

- `enabled` accepts declarations and new starts.
- `draining` loads installed declarations and preserves controls, recovery,
  joins, and Observe, but rejects new starts with HTTP 409 and stable code
  `CollectionWorkflowDraining`.
- `disabled` rejects declarations and starts. Observe is unavailable after the
  collection ledger is drained. Operators must enter `draining` and establish
  quiescence before selecting `disabled`.

Mode is immutable process configuration. Dispatch and simulation receive the
parsed value explicitly; simulation-visible code never reads the environment.

### Member identity is a shared public primitive

`temper-wasm-sdk` exposes the pure, versioned
`collection_member_id_v1(workflow_id, member_index, member_value)` helper. The
server calls the same implementation. Golden vectors cover both consumers so
applications can inspect bounded member results without copying kernel identity
logic.

### Existing Observe and telemetry become operational

Activation preserves the governed Observe routes and emits
`temper_collection_workflow_events_total`, active-window size, queue age,
member outcomes, retry events, terminal classification, and join latency.

## Rollout Plan

1. Activate and test the contract in an isolated preview with collection mode
   enabled.
2. Run the ARC v0.12 success, restart, partial-failure, cancellation, shortened
   proof-timeout, rejected-join, and authentic-import scenarios.
3. Merge and deploy Temper only after mandatory DST and code reviews pass.
4. Pin ARC to that merge, repeat verification, then publish ARC through Genesis.

## Readiness Gates

- Store and simulation parity remains green.
- At least 1,000 deterministic generated collection cases pass.
- Enabled/draining/disabled transitions and restart recovery pass.
- Sanitized ARC proof demonstrates no application cursor or retry loop.
- Preview Observe, metric, trace, and Datadog evidence is captured.

## Consequences

### Positive

- Applications can use the already-verified public contract.
- Rollback can quiesce durable work without losing controls or joins.
- One identity implementation serves both server and WASM applications.

### Negative

- Operators must drain before disabling or reinstalling an older runtime.
- ARC v0.12 imports cannot be resumed by v0.11.

### Risks

- A missed normal-dispatch seam could commit source state without workflow
  evidence. Atomic source/workflow tests and store parity guard this boundary.
- A mode applied inconsistently could strand work. Recovery and controls remain
  mode-independent in draining, and startup rejects invalid configuration.

### DST Compliance

Mode parsing happens at startup and is injected into deterministic paths.
Identity is pure SHA-256 over length-delimited inputs. Admission, recovery, and
joins keep ADR-0181's bounded ordered behavior.

## Non-Goals

- ARC parsing, counts, digests, claims, and publication semantics in Temper.
- Unbounded fan-out, application payload mapping, or application-controlled
  retries and timers.
- Resuming historical ARC v0.11 cursor imports.

## Alternatives Considered

1. **Add a generic sample app** — rejected because ARC supplies a bounded real
   workload and stronger cross-repository evidence.
2. **A boolean feature flag** — rejected because safe rollback requires a
   draining state that preserves controls and recovery while rejecting starts.

## Rollback Policy

Set mode to `draining`, wait until every workflow is terminal and every join is
delivered or explicitly acknowledged, then restart with `disabled`. Only after
that proof may operators reinstall the prior application/runtime release.
