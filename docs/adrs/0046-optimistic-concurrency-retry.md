# ADR-0046: Optimistic Concurrency Retry in Entity Actor Persistence

- Status: Accepted
- Date: 2026-04-16
- Deciders: Temper core maintainers
- Related:
  - `crates/temper-server/src/entity_actor/actor.rs` — `EntityActor::handle` / `persist_event` / `replay_events`
  - `crates/temper-runtime/src/persistence/mod.rs` — `PersistenceError::ConcurrencyViolation`

## Context

The entity actor handler is the single serialization point for actions against an entity. After evaluating a transition it calls `persist_event`, which appends a `PersistenceEnvelope` to the backing event store using `state.sequence_nr` as the expected-version guard. If another writer (for example, a Heartbeat action firing its own trigger cascade) appends before our envelope lands, the store returns `PersistenceError::ConcurrencyViolation { expected, actual }` and the actor rolls back its speculative state.

Today there is **no recovery on ConcurrencyViolation**: the handler rolls back, replies with an error, and the caller's action is *lost* — the LLM response in `ProcessToolCalls`, the `FinalizeResult` callback from `steering_checker`, or any other successful action that happened to race a same-tick heartbeat. In paw-foresight Runs 002-010, this manifested as sessions stuck in `Thinking` or `Steering` with populated `result` fields: the action ran, the state would have transitioned, but durability failed and the transition was thrown away.

OpenPaw is simultaneously removing the primary race vector (`heartbeat_typing` trigger on Heartbeat — see the OpenPaw track). Phase 2a here is defence-in-depth for unknown races with the same failure shape. The retry must not mask legitimate "action against a completed entity" failures, and must not paper over a deeper scheduler bug if conflicts become sustained.

## Decision

### Sub-Decision 1: Bounded retry on `ConcurrencyViolation`

Wrap the `EntityMsg::Action` persist path in a retry loop capped at **3 total attempts**. On a `ConcurrencyViolation`:

1. Roll back speculative in-memory state to the pre-action snapshot.
2. Call the existing `replay_events` to catch `sequence_nr` up to the authoritative store position.
3. Re-run `process_action_with_xref_and_field_mode` against the caught-up state. If the action is no longer legal (e.g., the entity reached a terminal state during the race), fail the caller immediately with the new error — do **not** suppress it. If it is still legal, it produces a new event (possibly against a different `from_status`).
4. Attempt `persist_event` again.

**Why this approach.** The retry is strictly scoped to one error variant. Replay + re-evaluation is the only way to produce a correct event — blindly re-appending the original event would violate the spec's state-machine semantics (the `from_status` may have changed). Re-running `process_action` against the caught-up state preserves determinism: the same spec + same inputs produce the same transition.

### Sub-Decision 2: Backoff schedule

Attempt 1 runs immediately. Attempt 2 sleeps **10 ms**, attempt 3 sleeps **50 ms**. No jitter.

**Why this approach.** Optimistic-concurrency conflicts here are single-process scheduler races, not distributed contention. 10 ms covers the tail of a Tokio reactor tick; 50 ms covers a scheduler-induced stall. Exponential-with-jitter is overkill and would add test nondeterminism for no gain in this failure mode.

### Sub-Decision 3: Observability

Expose three signals at the retry site:

- Counter `temper_entity_concurrency_retry_total{outcome=success|exhausted|action_illegal, entity_type}` — fires once per retry cycle.
- Histogram `temper_entity_concurrency_retry_attempts{entity_type}` — records the attempt count (1 through 3) when the handler completes a persist (successful or exhausted).
- Trace span `temper.entity.persist_with_retry` covering the retry loop; each attempt becomes a tagged log line with `attempt` and `outcome` fields.

**Why this approach.** The histogram at 1 = normal (no retries needed). Anything above 1 is a canary for a race we do not already understand. The alert threshold is intentionally set so a single retry per minute per entity triggers investigation — the correct response is to find the underlying cause upstream in the scheduler, not to raise the retry cap.

### Sub-Decision 4: TigerStyle assertion after replay

After each `replay_events` call in the retry loop, assert `state.sequence_nr == actual` (where `actual` comes from the `ConcurrencyViolation` error). This is a `debug_assert!` in the same style as the existing preconditions, and it catches the class of bug where the store says "I'm at sequence N" but replay only reaches N-k.

## Rollout Plan

1. **Phase 0 (this PR)** — Retry + metrics + DST race test.
2. **Phase 1 (follow-up, not in scope)** — Alert wiring in Datadog: page on sustained retry activity. Runbook: investigate the mailbox scheduler, not raise the retry cap.

## Consequences

### Positive
- Sessions no longer lose ProcessToolCalls / FinalizeResult callbacks to same-tick heartbeat races.
- Unknown future races with the same failure shape are absorbed with visibility.
- The metric makes contention quantifiable where it was previously invisible.

### Negative
- The entity handler's Action arm now contains a retry loop with backoff sleeps. Adds up to 60 ms of latency on the rare retry path; normal path is unaffected.
- `replay_events` is called inside the handler on the retry path, adding one additional round trip to the event store per failed persist.

### Risks
- **Retry could mask a real bug.** Mitigated by Sub-Decision 3: the metric makes retries observable, and the alert threshold treats sustained activity as a call to investigate the underlying race, not tune the retry.
- **Re-evaluated action diverges from original.** For guards that depend on entity state that changed during the race, the re-evaluated action may produce a different event or even fail. This is the correct behavior — the action sees the world as it is after the race — but it means callers must be prepared for the action to return a failure that wasn't present at first dispatch. This is already how the system behaves on guard failures, so no new caller surface is introduced.
- **Overflow-blob cleanup on retry.** If the first attempt wrote overflow blobs that the retry invalidates, the old blobs remain as orphans. Mitigated: blobs are content-addressed; identical payloads collapse. Divergent payloads become garbage that GC reclaims by the existing TTL policy.

### DST Compliance
- All state mutations happen through the same `process_action_with_xref_and_field_mode` path used today. No new randomness, no new `std::time::Instant::now()`.
- The backoff sleeps use `tokio::time::sleep` only on the *rare* retry path. Simulation tests that pause wall-clock time (via `tokio::time::pause()`) will advance deterministically. The sleep is annotated `// determinism-ok: rare retry backoff`.
- `replay_events` already exists and is DST-validated in pre_start; calling it in the retry path introduces no new determinism surface.

## Non-Goals

- **Retry for non-ConcurrencyViolation persistence errors.** Storage errors (serialization, backend failure) remain fatal — the caller sees them immediately.
- **Cross-entity coordination.** This ADR is about same-entity writer races only. Cross-entity consistency is a separate concern (see reactions ADRs).
- **Distributed contention.** If Temper grows a multi-writer deployment, this retry will need revisiting — the backoff schedule assumes single-process scheduler races.

## Alternatives Considered

1. **Single retry with no replay.** Rejected — the re-appended envelope would use a stale `from_status`, producing an invariant-violating event. The replay is not optional.
2. **Unbounded retry until success.** Rejected — an adversarial write pattern could starve callers indefinitely. 3 attempts with short backoff is enough for single-process scheduler races; sustained contention should surface as an alert.
3. **Exponential backoff with jitter.** Rejected — for sub-100ms intra-process races, the added nondeterminism hurts test reproducibility without meaningfully reducing conflict rates.
4. **Move retry up to the dispatcher.** Rejected — the dispatcher doesn't own `state_before` and doesn't know how to re-evaluate an action against replayed state. The actor is the correct owner.

## Rollback Policy

Remove the retry loop; the handler reverts to first-attempt-only behaviour. The counter and histogram can stay — they become zero-emission after rollback, which is harmless.
