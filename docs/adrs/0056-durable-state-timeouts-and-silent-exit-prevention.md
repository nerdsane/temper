# ADR-0056: Durable State Timeouts + Silent-Exit Prevention

- Status: Proposed
- Date: 2026-04-21
- Deciders: Temper core maintainers
- Supersedes: the "Non-durable under the MVP" caveat at `crates/temper-server/src/state/dispatch/state_timeouts.rs:145` introduced by ADR-0049
- Related:
  - ADR-0012: OAuth2 enablement (original ephemeral-scheduler context; this ADR closes its gap)
  - ADR-0048: Dispatch retry and error taxonomy (persistent store failures fall under its umbrella; this ADR adds the retry at the store layer)
  - ADR-0049: First-class state-entry timeouts (this ADR implements the durability contract ADR-0049 declared but MVP'd away)
  - ADR-0050: Mandatory liveness coverage (unchanged; this ADR makes the mandated timeouts actually fire reliably)
  - ADR-0051: Admission control (orthogonal; this ADR addresses in-flight durability, not arrival-rate)
  - ADR-0052: Instrumentation-as-policy (informs the `temper_integration_silent_exit_total` metric design)
  - openpaw ADR-0039 (orphaned-session-recovery.md): the consumer-side ADR that drove this discovery. The `ss-019db008` 2026-04-21 incident is the motivating evidence.
  - `crates/temper-server/src/entity_actor/actor.rs` (`pre_start` hook — primary edit).
  - `crates/temper-server/src/state/dispatch/state_timeouts.rs` (sibling to `arm_state_timeouts_if_needed`).
  - `crates/temper-server/src/state/dispatch/effects.rs` (silent-exit regression guard).
  - `crates/temper-store-turso/src/store/trajectory.rs` + `events.rs` (persist retry).

## Context

Three defects surfaced on 2026-04-21 when Session `ss-019db008-1b86-7e22-af25-3dfea0a43e84` (in openpaw) entered `Executing` state and stayed there indefinitely. The session's 300s state_timeout never fired; incoming Discord DMs queued in `steering_messages` but no WASM integration ran to consume them.

Reconstructed timeline (from openpaw logs):

1. 2026-04-20: Turso free-tier wrote-quota hit. `Operation was blocked: SQL write operations are forbidden` returned 409 on persist attempts. The openpaw dispatcher did not retry, propagating the 409 to callers.
2. A `monty_repl` invocation completed normally on the 2026-04-20 side. Its post-completion dispatch of `ProcessToolCalls` (or `HandleToolResults`) reached the dispatcher. The dispatcher attempted to persist the trajectory entry. Turso rejected with `BLOCKED`. The action dispatch failed; the entity's state was never advanced past `Executing`.
3. The Session's `Executing` state_timeout (300s, declared in `session.ioa.toml`) was armed when the session originally entered `Executing`. That tokio task was alive on the then-running container.
4. Between then and 2026-04-21, openpaw was redeployed multiple times (PRs #96-#100). Each redeploy replaced the container and killed the outstanding tokio timer. Per `state_timeouts.rs:145`:
   > Non-durable under the MVP — timers are lost across restarts.
5. On every re-hydration of the Session actor, `arm_state_timeouts_if_needed` was not called (it only runs inside `run_post_dispatch_effects`, i.e., on real action dispatch). The session was in `Executing` with a 300s timeout on paper and no clock anywhere in memory.
6. 2026-04-21: the operator restarts openpaw again, the orphan re-hydrates again, DMs arrive and route to `Session.Steer` (self-loop in openpaw's spec) which appends to `steering_messages` but triggers no integration. Paw silent.

Three failure surfaces at the Temper platform layer:

- **State timeouts do not survive actor passivation or server restart.** ADR-0049's "Non-durable under the MVP" comment is a known-incomplete implementation. Follow-up to durabilize was deferred.
- **Transient persist failures are not retried.** When Turso returns a transient `BLOCKED` or stream error, `temper-store-turso::store::trajectory::persist_trajectory_entry` and the events persist path bubble the error directly. The dispatcher has no retry layer; the integration's `on_failure` hook fires immediately. In the Turso-BLOCKED case, the action that WOULD HAVE advanced state is lost.
- **Integration invocations are not required to cause a state transition.** The dispatcher fires a `trigger` effect, awaits or spawns the WASM, and moves on. If the WASM returns without the entity's state changing — whether via WASM bug, persist failure, or integration completing without dispatching — the platform has no invariant that catches this.

## Decision

Close all three gaps at the platform layer. Each is a well-scoped fix with an independent safety benefit.

### Sub-Decision 1: State timeouts are re-armed on actor hydration

Extend `crates/temper-server/src/entity_actor/actor.rs::pre_start` so that after `replay_events` returns with the reconstructed state, the server evaluates the current state against its spec's `[[state_timeout]]` declarations and either fires the timeout immediately (if already overdue) or arms a timer for the remaining budget.

Implementation outline:

```rust
async fn pre_start(&mut self, ctx: &mut ActorContext<Self>) -> Result<Self::State, ActorError> {
    let state = self.replay_events(ctx).await?;
    self.server_state.arm_state_timeouts_on_hydration(
        &self.tenant,
        &self.entity_type,
        &self.entity_id,
        &state,
    );
    Ok(state)
}
```

New sibling fn in `crates/temper-server/src/state/dispatch/state_timeouts.rs`:

```rust
pub(crate) fn arm_state_timeouts_on_hydration(
    &self,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    state: &EntityState,
) {
    // For each [[state_timeout]] whose `state` matches state.status:
    //   compute state_entered_at = timestamp of the most recent event in
    //     state.events whose to_status == state.status && from_status != state.status
    //   let reset_ts = max of that and the timestamp of the latest event whose
    //     action ∈ reset_on
    //   let elapsed = now() - reset_ts
    //   if elapsed >= after_seconds → dispatch on_timeout action NOW (fire-and-forget)
    //   else → arm tokio task with (after_seconds - elapsed) remaining
}
```

State_entered_at is derivable from the event log (already persisted, already available post-replay). No new schema change.

**Why this shape:** the "true" durable scheduler would be a separate event-log-backed persistent queue (ADR-0049 Sub-Decision 3). That remains the correct long-term design. The hydration re-arm is the practical 80%-value prefix: timers survive the two cases that matter in production (actor passivation, server restart). A fully-event-log-backed scheduler can be added later without changing this hook's semantics.

### Sub-Decision 2: Persistence retries on transient errors

In `crates/temper-store-turso/src/store/trajectory.rs` and the events-persist path, wrap the Hrana `execute` calls in an exponential-backoff retry:

```rust
const RETRY_DELAYS_MS: [u64; 4] = [250, 500, 1000, 2000];

async fn execute_with_retry(...) -> Result<...> {
    let mut last_err = None;
    for (attempt, delay_ms) in std::iter::once(0).chain(RETRY_DELAYS_MS.iter().copied()).enumerate() {
        if delay_ms > 0 { tokio::time::sleep(Duration::from_millis(delay_ms)).await; }
        match hrana_execute(...).await {
            Ok(r) => {
                if attempt > 0 {
                    metrics::record_turso_write_retries(reason_code(&last_err), attempt);
                }
                return Ok(r);
            }
            Err(e) if is_transient(&e) => { last_err = Some(e); continue; }
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap())
}
```

`is_transient` matches: Hrana `BLOCKED` code, stream-error, connection reset, timeout. Non-transient (e.g., SQL syntax errors) propagate immediately.

Event sourcing is idempotent per-`(entity, seq)` — the store already deduplicates on that key, so retrying a write that actually succeeded but whose ACK was lost is safe by construction.

New metric `temper_turso_write_retries_total{reason, attempt}` exposes retry pressure for operators.

### Sub-Decision 3: Silent-exit regression guard

In `crates/temper-server/src/state/dispatch/effects.rs`, after the inline-WASM dispatch path that awaits an integration's response, detect the "integration returned without any state transition" case:

```rust
let pre_status = response.state.status.clone();
// ... dispatch WASM integration (existing code) ...
if let Some(ref post_response) = inline_response
    && post_response.state.status == pre_status
    && integration.kind == IntegrationKind::Trigger
{
    tracing::warn!(
        target: "temper_server::integration",
        entity_type = ctx.entity_type,
        entity_id = ctx.entity_id,
        action = ctx.action,
        integration = integration.name,
        state = %pre_status,
        "integration returned without state transition — invariant violation"
    );
    runtime_metrics::record_integration_silent_exit(
        ctx.tenant.as_str(),
        ctx.entity_type,
        integration.name.as_str(),
    );
}
```

Under healthy operation `temper_integration_silent_exit_total` is permanently zero. The consumer (openpaw) is responsible for enforcing the invariant at the WASM layer (per openpaw ADR-0039 Sub-Decision 3a). This platform-side check is a belt-and-suspenders regression guard — if the WASM-side invariant ever regresses, operators see the counter go nonzero and can act before user-visible impact.

**Why no automatic `on_silent_exit` recovery field on actions:** scope control. The consumer-side invariant + persist retries close the common causes. A declarative recovery primitive is appealing but adds spec grammar and requires usage-pattern data we don't have yet. Revisit once we have production evidence that this regression-guard fires — at that point we'll know what shape of recovery operators actually want.

## Readiness Gates

- `cargo test -p temper-server` — new tests in `state_timeouts.rs` covering: hydration into overdue-Executing fires TimeoutFail immediately; hydration into not-yet-overdue Executing arms timer with remaining budget.
- `cargo test -p temper-store-turso` — new tests for transient-error retry exhaustion + idempotent success on retry.
- `cargo test --workspace` — full workspace passes (note: pre-push hook runs this; historically 60+ min on `dst_platform_random`).
- 24h production observation post-deploy: `temper_integration_silent_exit_total{service:openpaw}` remains zero. `temper_turso_write_retries_total` non-zero under Turso load but max attempt ≤ 2 in normal ops.
- `temper_state_timeout_fired_total` per-entity-type metric: does NOT spike during the first 10 min post-deploy. (A modest bump is expected as the hydration re-arm clears orphans that had accumulated pre-fix; a sustained spike would indicate a bug.)

## Consequences

### Positive
- State timeouts survive actor passivation and server restart. Declarative liveness becomes real, not aspirational.
- Transient persist failures self-correct rather than corrupting entity state. One fewer class of data-integrity-adjacent bugs.
- Silent integration exits are detected at the platform layer, creating an observable invariant that can be alerted on before user-visible impact.
- Openpaw ADR-0039 becomes implementable — Sub-Decisions 2 and 3b/3c of that document live here.
- Every Temper-native app benefits, not just openpaw.

### Negative
- `pre_start` slightly slower on cold start (extra event-log walk to compute `state_entered_at`). Bounded by the number of `[[state_timeout]]` declarations × constant per-decl cost; in practice sub-millisecond.
- Turso retries add up to ~3.75s of latency to writes that hit transient errors. Acceptable — the alternative is user-visible action failure.
- Three new code paths to maintain. Each is narrow and well-tested.

### Risks
- **Hydration-re-armed TimeoutFail bursts on first deploy.** Every Session actor currently orphaned in `Executing` (or similar) will fire `TimeoutFail` on first hydration after this deploys. Expected. Operators should expect a brief spike of `state_timeout_fired_total` during the first 10 min post-deploy, then drop to steady-state. Dashboard annotation recommended.
- **Event-log walk cost scales with session lifetime.** Sessions with tens of thousands of events could take a few ms to compute `state_entered_at`. If profiling shows this matters, cache `state_entered_at` as a materialized field (future optimization; not blocking).
- **Silent-exit metric false-positives if an integration legitimately doesn't need to transition.** Not applicable for `trigger` integrations (which by contract drive forward progress). But if a future integration type emerges that fires side-effects without state change (e.g., a pure notification webhook), the check must exclude it. Currently gated on `integration.kind == Trigger` so webhook-like integrations are already excluded.

## Non-Goals

- Full event-log-backed durable scheduler (ADR-0049 Sub-Decision 3). Still the correct long-term design; hydration re-arm is the 80%-value prefix. Revisit once production data shows the remaining 20% matters.
- Declarative `on_silent_exit` Action field. Deferred pending production-usage data.
- Cross-entity recovery cascades. Orthogonal to this class of bug.
- Re-arming state_timeouts from a *proactive* background scan (e.g., "every 5 min, check all passivated entities for overdue timeouts"). The hydration hook covers the case when an entity is accessed; a proactive scan would cover entities that are never accessed again. Defer — those entities are dead weight anyway; the state_timeout's point is to release resources when something DOES arrive for them.

## Alternatives Considered

1. **Fully durable event-log-backed scheduler now** (ADR-0049 Sub-Decision 3 as originally planned). Rejected for this ADR — larger scope, longer review cycle, blocks the openpaw orphan fix. Still the right long-term answer; this ADR is a staging post.
2. **Persistent timer table in Turso separate from event log.** Rejected — adds a second source of truth for "what's scheduled," creates consistency burden, duplicates what event-replay can already derive.
3. **Rely on admission control (ADR-0051) to prevent orphans.** Rejected — admission caps arrival rate, does not catch in-flight failures. Orthogonal concern.
4. **Skip silent-exit detection, rely entirely on consumer-side WASM invariants.** Rejected — every consumer would have to replicate the invariant. Better to have a platform-level check that fires when consumer invariants regress, as a regression guard.

## Rollback Policy

- Revert commits in reverse order: silent-exit detection → hydration re-arm → Turso retry → ADR.
- `temper_integration_silent_exit_total` can be queried from reverted data; the metric historical values remain valid.
- The "Non-durable under the MVP" comment restoration is one-line trivial.
- No persistent-state migration required. Entity snapshots and event logs are forward- and backward-compatible — the fix adds behaviour on the code side, not the data side.
