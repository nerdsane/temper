# ADR-0177: Single-owner simulation delivery

- Status: Proposed
- Date: 2026-08-23
- Deciders: Temper core maintainers
- Related:
  - [Fork issue #24](https://github.com/nikstern/temper/issues/24)
  - [Upstream PR #404](https://github.com/nerdsane/temper/pull/404)
  - ADR-0174: Faithful deterministic actor recovery
  - Fork PR #1: Durable observable entity reactions
  - `crates/temper-runtime/src/scheduler/`
  - `crates/temper-verify/src/simulation.rs`

## Context

`SimScheduler::tick` currently gives each due message two owners: it enqueues the
message in the target mailbox and returns a clone to the caller. Runtime and L2
verification drivers process the returned clone but do not consume the mailbox.
Processed messages therefore remain queued, while a final unobserved `tick` can
enqueue deliveries whose returned clones are discarded. Handler and integration
callback rejection can also disappear without making the run fail.

Upstream PR #404 fixes the original defect, but predates fork PR #1 and ADR-0174.
The fork now has queued actor-to-actor delivery, deterministic crash/restart
reconstruction, and durable-reaction tests. A straight cherry-pick would regress
those capabilities and would not route `run_queued` through the corrected
ownership contract.

## Decision

### Tick only transfers pending messages into mailboxes

`SimScheduler::tick` advances logical time and moves due messages from the pending
queue into target mailboxes. It returns no messages. At every point, a scheduled
message has one owner: the pending queue, a mailbox, or the consumer that removed
it.

### Drivers own deterministic draining and bounded quiescence

`drain_ready` removes all mailbox messages in actor-id order and FIFO order within
each mailbox. Runtime random exploration, queued delivery, and L2 verification all
use the same cycle: tick, reconstruct restarted actors where applicable, drain,
then apply each drained message once.

Each driver flushes pending queues and mailboxes through that cycle until
quiescent or an explicit tick budget is exhausted. Budget exhaustion is surfaced
as a failure; it is never reported as quiescence. The scheduler-level
`run_until_quiescent` helper is removed because a scheduler cannot make a
non-empty mailbox quiescent without consuming messages, which belongs to the
driver.

Drivers permit at most one in-flight action per actor. This prevents a later
action from being selected against state that an earlier scheduled action has
not updated yet.

### Crash and restart boundaries preserve single ownership

Restart injection runs at the beginning of a tick, independently of whether a
message is due for the actor. Runtime drivers reconstruct restarted actor state
before draining that tick's mailboxes. Post-delivery crash injection runs in a
separate `finish_tick` boundary after drained messages have been applied, so a
handler never executes while its actor is marked crashed.

### Failed consumption is observable

A drained message that names an unknown actor or whose handler rejects it becomes
a simulation violation. A configured integration callback that is rejected also
becomes a violation. Deliberate scheduler drops caused by drop, crash, or unknown
target fault behavior remain recorded in the scheduler's dropped log and counts.

Integration callbacks are scheduled into the same mailbox path as actor actions.
They therefore share the delay, drop, crash/restart, ordering, and quiescence
budget semantics instead of bypassing the scheduler through a separate vector.

### L2 reaches means visited during the trace

Applying previously lost tail messages exposes an unrelated calibration error:
L2 currently evaluates `reaches` only against the final state. For cyclic models,
eventual reachability is satisfied when the target is visited at any point, even
if the actor later leaves it. L2 records visited statuses and evaluates `reaches`
against that trace history; genuinely unreachable targets still fail.

## Rollout Plan

1. Add seeded regression tests that reproduce retained mailboxes, lost tails, and
   silent callback/handler failures on the old ownership model.
2. Change the scheduler contract and port every runtime and verifier caller.
3. Exercise delay, duplicate sends, crash/restart, unknown targets, handler
   rejection, and callback rejection under deterministic budgets.
4. Run the full workspace, verification cascade, randomized DST, and fork durable
   reaction suites before merge and deployment verification.

## Consequences

### Positive

- Mailbox depth describes unconsumed work instead of retaining processed clones.
- Delayed deliveries at the simulation horizon are applied or reported as budget
  exhaustion rather than silently discarded.
- Runtime and verification drivers exercise one delivery contract.
- Failed consumption and callbacks cannot leave a falsely green run.

### Negative

- Corrected runs may execute more transitions and produce different traces for an
  existing seed because previously lost deliveries are now applied.
- Drivers must explicitly own draining and budget accounting.

### Risks

- Draining after several ticks could apply a message enqueued before a later actor
  crash. Drivers therefore drain after every tick and tests pin crash/restart
  behavior.
- A budget smaller than an injected delay can end a run before quiescence. That is
  reported explicitly instead of being mistaken for successful completion.

### DST Compliance

- Mailboxes remain a `BTreeMap`, preserving actor-id order; each mailbox remains
  FIFO.
- Logical time and the seeded scheduler PRNG remain the only time and randomness
  sources.
- Flushes consume explicit tick budgets and introduce no wall clock, threads, or
  ambient I/O.
- No determinism suppression annotations are required.

## Non-Goals

- Changing production actor delivery or durable reaction semantics.
- Adding a new probabilistic duplicate-message field to `FaultConfig`; duplicate
  delivery is covered by scheduling two distinct messages with the same payload,
  preserving the scheduler's unique message ownership invariant.

## Alternatives Considered

1. **Return owned messages from `tick` and delete mailboxes.** Rejected because
   mailbox inspection and per-actor receive semantics are useful simulation
   observables and part of existing tests.
2. **Drain inside `tick`.** Rejected because time advancement and consumption are
   separate state transitions, and drivers need to own application and failure
   reporting.
3. **Keep `run_until_quiescent` and drain inside it.** Rejected because the
   scheduler cannot apply messages or report handler failures; quiescence is a
   driver-level operation.

## Rollback Policy

Revert the scheduler and all driver changes together. Mixing an enqueue-only tick
with clone-processing callers, or a clone-returning tick with mailbox-draining
callers, violates the single-owner invariant.
