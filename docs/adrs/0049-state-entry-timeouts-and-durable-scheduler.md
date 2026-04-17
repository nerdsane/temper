# ADR-0049: First-Class State-Entry Timeouts and Durable Scheduler

- Status: Proposed
- Date: 2026-04-17
- Deciders: Temper core maintainers
- Related:
  - ADR-0012: OAuth2 enablement (introduced `schedule` effect; this ADR upgrades its durability contract)
  - ADR-0048: Dispatch retry and error taxonomy (parallel hardening)
  - ADR-0050: Mandatory liveness coverage (enabled by this primitive)
  - `crates/temper-spec/src/automaton/types.rs` (spec surface)
  - `crates/temper-server/src/state/dispatch/effects.rs` (execution)
  - `crates/temper-server/src/entity_actor/actor.rs` (timer heap, replay)

## Context

Every lifetime cap in every shipping entity spec today is hand-rolled from three ingredients: a counter state variable, a `{ type = "schedule", action = "...", delay_seconds = N }` effect on some "keep-waiting" action, and a guard that stops the loop. Examples: Session's `max_provision_checks=24`, `recovery_count<3`, `max_follow_ups=100`.

This pattern has two failure modes:

1. **Spec authors miss states.** The 2026-04-17 Katagami incident traced to Session's `Provisioning` state having no `TimeoutFail` entry in its `from` list — the author wrote per-state counters for some states but omitted Provisioning entirely. Under load, sessions got stuck in Provisioning with no way to self-recover; the heartbeat_scan watchdog tried to apply `TimeoutFail` but was rejected by the state machine.
2. **Schedules are ephemeral.** ADR-0012 explicitly documents (line 138): *"Production timer scheduling via tokio::spawn is not durable across server restarts. Timers in flight are lost if the server restarts."* The mitigation is "check on replay if the deadline passed" — but that check is the author's responsibility and is not enforced.

Both failures trace to the same cause: **liveness is not a first-class property in the spec language**. It is a side-effect of disciplined coding, and discipline lapses.

## Decision

Introduce `[[state_timeout]]` as a first-class TOML construct. Back it with a durable scheduler whose timers survive restarts. Make `reset_on` progress signals a declarative property so the hand-rolled heartbeat-watcher pattern becomes unnecessary.

### Sub-Decision 1: TOML surface

```toml
[[state_timeout]]
state = "Provisioning"
after_seconds = 180
on_timeout = "TimeoutFail"
max_occurrences = 1                  # default 1
reset_on = ["ProvisionPending"]      # optional; progress signals
params = { error_message = "provisioning did not complete within 180s" }
```

Semantics: when the entity enters `state`, schedule `on_timeout` to fire `after_seconds` later. If any action in `reset_on` fires while still in `state`, cancel and re-arm. If the entity leaves `state` for any reason before firing, cancel.

**Why this shape**: Declarative. Complete per state. Co-located with the state it governs. Replaces counter + schedule + guard triplets with one block whose semantics can be reasoned about without reading the rest of the spec.

The compiler auto-generates two state variables per `state_timeout`:
- `{state}_entered_at: timestamp` — written on state entry
- `{state}_timeout_seq: counter` — bumped on each re-arm

Authors never touch these; the runtime does.

### Sub-Decision 2: `reset_on` replaces heartbeat-watcher patterns

Today's Session.ioa.toml ships a companion `HeartbeatMonitor` automaton whose only job is to scan for sessions that haven't heartbeat-ed in N seconds. With `reset_on`, the same property becomes a property of each state: "this state resets its timeout when any of these actions fires."

**Why this approach**: one watchdog per state, co-located with its definition, auto-cancelled on state exit. Two competing watchdogs (state timeout + heartbeat_scan) would race to fire `TimeoutFail` and diverge on edge cases. One declarative primitive is the less-wrong design.

Consumed by C1 (ADR-0036 in openpaw) which deletes `heartbeat_scan` entirely.

### Sub-Decision 3: Durable scheduler via event log

Every schedule becomes three possible events on the entity's event log:

- `ScheduledActionCreated { seq, deadline_ms, action, params }` — appended when a timer is armed.
- `ScheduledActionFired { seq }` — appended when the timer fires (either at wall-clock deadline or on replay as overdue).
- `ScheduledActionCancelled { seq }` — appended on state exit or `reset_on` re-arm.

On server boot, event-log replay reconstructs entity state. For each `ScheduledActionCreated` whose `seq` is not followed by Fired or Cancelled:
- If `deadline_ms` is in the future: re-arm with remaining delay.
- If `deadline_ms` is in the past: dispatch immediately with `overdue = true` attribute on the action.

**Why event-sourced timers**: the event log is already the source of truth for entity state. Timers that aren't events aren't crash-safe, full stop. ADR-0012's "check on replay" mitigation becomes the actual mechanism instead of a post-hoc patch.

### Sub-Decision 4: Sequence-based cancellation, single heap per actor

Race condition to handle: timer fires at the same instant the entity transitions out of the state. Both the fire and the exit write to the same actor; one must no-op.

Resolution: each arm bumps `{state}_timeout_seq`. When the fire callback arrives, it checks `entity.state == my_state && entity.{state}_timeout_seq == my_seq`. Either miss → drop silently.

Implementation: one tokio task per entity actor (not per-timer) owns a `BinaryHeap<Timer>` of outstanding timers for that entity and sleeps until the earliest. Re-arms replace the earliest entry.

**Why per-actor heap**: avoids thousands of idle tasks under high entity counts. Per-actor already passivates on idle (ADR-0028), so timer ownership naturally follows actor ownership.

### Sub-Decision 5: Backward compatibility for `schedule` effect

Existing `{ type = "schedule", action = "X", delay_seconds = N }` effects (ADR-0012) route through the same durable scheduler but without `state_timeout` semantics — they fire unconditionally when the deadline hits, regardless of current state. Same durability upgrade, same event-log backing.

**Why**: migrating every `schedule` effect to `state_timeout` would be spec churn without benefit. They coexist.

## Rollout Plan

1. **Phase 0 (ADR-approved)** — Spec parser accepts `[[state_timeout]]` blocks. Metadata carries them. No runtime behavior yet.
2. **Phase 1** — Durable scheduler lands. Existing `schedule` effects migrated to event-log backing. Restart test: timers survive server bounce.
3. **Phase 2** — `state_timeout` runtime execution ships. No specs declare any yet.
4. **Phase 3** — First consumer: Session spec (ADR-0036 in openpaw). Tight canary on one tenant.
5. **Phase 4** — Fleet-wide spec migrations (see ADR-0050 liveness rule).

## Readiness Gates

- Restart test: kill server with 100 pending timers; verify every timer fires at its original deadline (±100ms) post-restart.
- `temper_scheduler_overdue_on_replay_total` stays at zero during normal operation; spikes only on deploys.
- DST: deterministic reproduction of timer-fire-exactly-at-state-exit race, 100 iterations, no divergence.

## Consequences

### Positive
- Liveness becomes a declarative property of the spec, not emergent behavior.
- Restart semantics are predictable. Deploy bounces no longer silently lose timers.
- `reset_on` eliminates the need for a separate liveness-monitor entity per domain.

### Negative
- Event log grows. Each timer is three events: Created, (Cancelled | Fired). Cost: small relative to existing action events; bounded by per-entity timer count.
- Spec surface grows. One more section authors must understand. Mitigated by ADR-0050 making it mandatory, which eliminates the "is this worth it?" per-author decision.

### Risks
- **Replay cost under crash recovery.** Entity with 1000 past timers replays all of them. Mitigation: compact entity state snapshots (ADR-0028 already handles this for current fields; timers follow the same snapshot discipline).
- **Wall-clock drift.** Scheduled actions compare wall-clock to deadlines. Servers with bad clocks fire early/late. Mitigation: use monotonic clock for interval measurement where possible; deadline stored as absolute `now()` + offset.

### DST Compliance
- Timer heap keyed on `sim_now() + delay`.
- Replay under DST walks the deterministic event log in order; firings produce deterministic follow-up events.
- New determinism-ok annotations around the per-actor timer task (required because it holds an async sleep).

## Non-Goals

- Cross-entity timer coordination ("fire X when entity A reaches state S").
- Interval timers (cron-like recurring schedules). Use ADR-0021 `/schedule` CronCreate instead.
- Priority-heap scheduling (first-in-time is good enough).

## Alternatives Considered

1. **Keep schedule-effect patterns, just add a linter** — Rejected. Lints catch common shapes, not all shapes; the primitive is the fix.
2. **External timer service (Temporal, etc.)** — Rejected. Operational cost; determinism loss; violates the "everything is entities + events" thesis.
3. **Per-timer tokio tasks instead of per-actor heap** — Rejected. Runs into task-spawn overhead at scale; loses locality.

## Rollback Policy

Hard to reverse once specs declare `[[state_timeout]]`. Fallback: emit a `deprecated: revert-to-ADR-0012-semantics` compatibility mode that re-enables the old hand-rolled patterns. Not planned to implement unless we hit an unexpected DST divergence the durable scheduler introduces.
