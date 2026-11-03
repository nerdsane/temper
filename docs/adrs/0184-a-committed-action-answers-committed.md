# ADR-0184: A Committed Action Answers Committed, and Its Triggers Are Independent

- Status: Accepted
- Date: 2026-11-01
- Deciders: Temper core maintainers
- Supersedes: ADR-0152 Sub-Decision 2 (inline failure becomes `success: false`), for actions that are not composite
- Related:
  - ADR-0140: A composite action fails when its integration fails
  - ADR-0152: Integration failure is never silent
  - `crates/temper-server/src/state/dispatch/trigger_failures.rs`
  - `crates/temper-server/src/state/dispatch/effects.rs` (inline integrations)
  - `crates/temper-server/src/state/dispatch/wasm.rs`, `adapter.rs` (trigger loops)

## Context

An action's triggers run after its transition is durable. ADR-0152 made a trigger that fails with no `on_failure` never silent: in the background a compensating transition runs, and inline the caller was answered `success: false`, which the OData route answers 409 `ActionFailed`.

That inline answer said something false. The action had committed, but the caller was told it failed. Several other things followed from it:

- The committed action's entity reactions did not fire, because reactions start only on a successful answer. One action declared an integration and then a reaction; when the integration failed, the reaction was skipped.
- The transition's spawn requests were dropped, though its scheduled actions were armed.
- No compensation ran inline, so the record showed nothing of the failure.
- A refused answer from an integration replaced the committed action's answer, so a caller whose action committed got 409.

The trigger loops also stopped at the first failing integration. A failing WASM trigger skipped every WASM trigger after it, and inline it skipped every adapter trigger as well. Triggers declared on one action are independent; no spec should rely on one stopping another.

## Decision

### Sub-Decision 1: every trigger runs

The WASM and adapter loops run every trigger the action declares. A failure is kept and the triggers after it still run. When any failed, the loop returns all of their errors together. Inline, a WASM failure no longer skips the adapter triggers.

**Why this approach**: triggers on one action have no order contract. A spec that needs one step to depend on another's result expresses it as a reaction to that step's answer, which runs only when the answer lands.

### Sub-Decision 2: an inline caller is answered committed

When an action that is not composite committed and one of its inline triggers failed, or an integration's answer was refused:

- the answer is `success: true`, with the record as it now stands;
- `EntityResponse::trigger_failures` names each failure, and the OData action answer carries them as `@temper.triggerFailures`;
- the action's reactions, spawn requests and scheduled actions run as for any committed action;
- a trigger that failed is compensated as in the background (ADR-0152 Sub-Decision 3): the record's declared failure step runs, or an `integration_failure_dropped` event and metric are recorded. Several failures compensate once, with every error named;
- a refused answer is reported only. Its refusal is already on the record, and compensating it could undo newer state.

A caller's retry under the same idempotency key gets the same committed answer and runs no trigger again. A failed trigger is recovered through the record's failure step, not by the caller sending the action again.

### Sub-Decision 3: a composite still fails

A composite action's effect is its integration's sub-writes (ADR-0140), so when that integration fails, the action has failed. Its caller gets `success: false` as before. A table that cannot be read counts as composite, so a failed integration is never answered as success.

## Consequences

- An OData caller waiting on an action gets 200 whenever the action committed, and reads `@temper.triggerFailures` to learn which triggers failed.
- A spec that relied on a later trigger not running after an earlier one failed must move that step to a reaction on the earlier trigger's answer. A sweep of the specs on disk found one such spec, a paw-patrol `Effort.Merge` copy, which hangs its deploy reaction on the merge itself rather than on the merge check's answer.
- Composite actions are unchanged.
