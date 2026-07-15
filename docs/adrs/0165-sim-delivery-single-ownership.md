# ADR-0165: Simulation Delivery Has a Single Ownership Path

## Status

Accepted (2026-07-14)

(Numbered 0165: 0156–0164 are claimed by concurrently open arena branches.)

## Context

The deterministic scheduler exposed two incompatible delivery contracts:
`Scheduler::tick()` both enqueued a due message into the target mailbox AND
returned a clone. The simulation drivers (temper-runtime's
`SimActorSystem::run_random`, temper-verify's model-checking driver)
processed the returned clones and never drained mailboxes — `receive()` had
zero production callers — and each loop iteration ended with a bare
`tick()` whose returned deliveries were discarded. Consequences (ARN-236,
all reproduced by seeded tests): every processed message remained queued in
its mailbox forever; deliveries surfaced only by the trailing tick (delayed
messages coming due after the last driver iteration) were silently lost —
seed 0 of the regression sweep loses 14 of 44 sends; and a rejected
integration callback (`let _ = self.step(...)`) left the run green. DST and
model-check results therefore did not faithfully exercise the schedule they
claimed to.

## Decision

1. **`tick()` advances logical time and enqueues only.** It returns
   nothing. A message is owned by exactly one place at every instant: the
   pending queue, a mailbox, or the consumer that drained it.
2. **One consumption path.** `drain_ready()` removes and returns all queued
   messages in deterministic order (actor-id order, FIFO per mailbox);
   `receive()` remains for single-actor consumption. Drivers apply each
   drained message exactly once (`apply_delivered_message`, extracted so
   the main loop and the flush share one application path). The clone-return
   processing path is deleted.
3. **Budgeted schedule flush.** After the driver's action loop, a bounded
   flush (budget = `max_ticks`) ticks and drains until quiescent, so
   delay-faulted deliveries due after the last iteration are applied through
   the same exactly-once path instead of being discarded.
4. **Callback failures are part of the result.** A rejected integration
   callback is recorded as a violation (`integration callback rejected:
   ...`) on the simulation result; a run that discards one cannot stay
   green. Both drivers share the runtime scheduler, so the verifier and
   runtime exercise the same delivery contract.

## Consequences

- Exactly-once delivery is now a tested property: a 50-seed sweep with delay
  faults asserts applications == sends; a fault-free run asserts empty
  mailboxes and quiescence; a rejected callback asserts a non-green result.
- `is_quiescent()` (pending + mailboxes empty) is now reachable in driver
  runs — previously mailboxes never emptied, so quiescence was unreachable
  by construction.
- Runs get slightly longer traces: deliveries that were silently lost are
  now applied (that is the point). Seeds produce different — now correct —
  transition sequences than before; recorded-run comparisons across this
  change are not byte-compatible, which is expected for a semantics fix.
- `Scheduler::run_until_quiescent` keeps its shape (tick in a bounded loop),
  but with tick enqueue-only nothing drains during it — once anything is
  enqueued it degrades to "tick `max_ticks` times" and terminates via the
  bound, never via quiescence. Its only callers are scheduler unit tests,
  which drain after it returns.

## Alternatives Considered

- **Making `tick()` return owned (non-cloned) messages and deleting
  mailboxes from the delivery path**: also a single-owner model, but it
  removes per-actor mailbox semantics (depth inspection, per-actor
  `receive`, crash-time mailbox behavior) that other tests and fault
  injection rely on. Enqueue-then-drain keeps those observables.
- **Draining inside `tick()` (tick returns owned mailbox contents)**:
  conflates time advance with consumption; a driver that wants to tick
  several times before applying (e.g. coalescing) couldn't. Separate verbs
  keep the contract explicit.
