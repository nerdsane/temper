# ADR-0171: Single-owner simulation delivery

- Status: Proposed
- Date: 2026-07-13
- Deciders: Temper core maintainers
- Related:
  - ARN-236: Simulation delayed-message ownership correctness
  - `crates/temper-runtime/src/scheduler/core.rs`
  - `crates/temper-runtime/src/scheduler/sim_actor_system.rs`
  - `crates/temper-verify/src/simulation.rs`

## Context

The deterministic scheduler currently gives every due message two owners. `SimScheduler::tick` enqueues a message in the target mailbox and also returns a clone. The runtime actor simulator and the verifier simulator process the returned clone without consuming the mailbox entry. Both drivers then perform a second tick whose returned messages are ignored. A delivery can therefore be applied while its mailbox copy remains queued, or be moved from the pending heap into a mailbox by the ignored tick and never be applied.

Integration callback delivery has a related truthfulness gap. The actor simulator recursively invokes callback actions and discards their errors, so a rejected callback can still yield a successful simulation result.

These paths make deterministic simulation disagree with the delivery contract it claims to verify. The correction spans the shared scheduler and both simulation drivers, so it requires an architectural decision before implementation.

## Decision

### A scheduler tick never transfers processing ownership

`SimScheduler::tick` advances logical time, applies deterministic faults, and moves due messages from the pending heap into actor mailboxes. It does not return message clones.

### One deterministic mailbox drain owns processing

The scheduler exposes one budgeted drain operation. It consumes ready messages from mailboxes in `BTreeMap` actor order and FIFO order within each actor. Both the runtime actor simulator and verifier simulator process only messages returned by this consuming drain. No driver may inspect a delivery through a parallel return path.

The drain accepts a message budget. A tick budget already bounds elapsed logical time; runtime and verifier configurations make the per-tick message budget explicit. Reaching a budget preserves undrained messages for a later tick rather than dropping them.

### Reactions are iterative, budgeted, and fallible

Integration callbacks are drained iteratively from their reaction queue under an explicit per-tick reaction budget. Callback rejection is recorded as a simulation execution error and makes the run unsuccessful. Callback dispatch does not recursively start another independent drain.

### Simulation success includes delivery execution

A successful simulation requires both invariant preservation and absence of delivery/callback execution errors. Results retain explicit error evidence so callers can distinguish a modeled invariant violation from a driver failure.

## Rollout Plan

1. Add behavioral regressions against the current competing-ownership behavior.
2. Change the scheduler contract and migrate both in-repository simulation drivers together.
3. Add deterministic delayed-delivery and callback-failure coverage, then run the full verification cascade.

## Readiness Gates

- A processed message leaves no mailbox clone behind.
- Multiple actors and multiple due messages drain in reproducible actor/FIFO order.
- A delivery becoming due on the final observed tick is consumed, not discarded.
- Callback rejection is visible in the result and fails verification.
- Runtime and verifier compile against the same consuming scheduler API.

## Consequences

### Positive

- Every ready message has one processing owner and one consumption point.
- Delivery order remains deterministic and replayable.
- Budget exhaustion defers work without silently losing it.
- Simulation results report callback rejection truthfully.

### Negative

- Callers that used the vector returned by `tick` must migrate to the consuming drain.
- A per-tick budget can defer ready work to a later tick; callers must size budgets for their explored workload.

### Risks

- A drain order change can alter existing seeded traces. Deterministic replay remains stable after the contract change, and regression tests pin the new ordering.
- Too-small budgets can reduce exploration depth. Defaults cover the configured actor/action bounds, and remaining work is retained rather than discarded.

### DST Compliance

- Scheduler collections remain `BTreeMap`, `BinaryHeap`, and `VecDeque`; no nondeterministic iteration is introduced.
- Logical time and the seeded scheduler RNG remain the only time and randomness sources.
- No threads, wall-clock calls, ambient I/O, or new determinism exceptions are introduced.

## Non-Goals

- Changing production actor mailbox semantics.
- Changing the probability or ordering of configured scheduler faults.
- Executing real external integrations inside deterministic simulation.

## Alternatives Considered

1. **Process only the vector returned by `tick` and remove mailboxes** — Rejected because the scheduler's mailbox is the natural ownership boundary and is already required by receive/quiescence semantics.
2. **Keep both paths and explicitly remove returned clones from mailboxes** — Rejected because clone correlation adds a compensating protocol while preserving two owners.
3. **Let each simulator implement its own mailbox iteration** — Rejected because independent drivers can drift again and duplicate ordering/budget logic.

## Rollback Policy

Revert the scheduler and both driver migrations together. A partial rollback is invalid because it would restore competing ownership or leave one simulator unable to consume deliveries.
