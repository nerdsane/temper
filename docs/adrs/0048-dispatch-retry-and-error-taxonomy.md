# ADR-0048: Dispatch-Layer Retry and Error Taxonomy

- Status: Proposed
- Date: 2026-04-17
- Deciders: Temper core maintainers
- Related:
  - ADR-0168: Optimistic-concurrency retry (pattern reference; different scope)
  - ADR-0028: Memory-bounded lazy hydration and passivation (interaction with cold-start latency)
  - `crates/temper-server/src/state/dispatch/actions.rs` (primary change site)
  - `crates/temper-runtime/src/actor/actor_ref.rs`
  - `crates/temper-runtime/src/actor/errors.rs`
  - `crates/temper-runtime/src/mailbox/mod.rs`

## Context

Every entity mutation in Temper goes through `ActorRef::ask` with a single 5-second budget (`TEMPER_ACTION_TIMEOUT_SECS`, default defined in `state/mod.rs:166-173`). Mailboxes are bounded at 1000 with non-blocking `try_send` (`mailbox/mod.rs:54-58`). On transient failure the dispatch layer returns a hard error; retry, if it happens, lives in each caller.

Two production incidents on 2026-04-17 demonstrate the hole this leaves:

- **Railway paw**: `POST /Files` (content file create) returned `HTTP 500 "Actor query failed: ask timeout after 5s"`. One slow blob write upstream of an actor turned into a user-visible 500.
- **Katagami bulk regenerate**: 11 concurrent Session creations produced 8 `actor dispatch failed: ask timeout after 5s` errors. CurationJobs retried manually (production logs show `error_message: "Retrying after actor timeout"` on eventually-completed jobs).

The production evidence is unambiguous: callers already retry; the retry logic just lives in the wrong place, gets rewritten per call-site, and leaks as 500s to end users before anyone catches it.

ADR-0168 added optimistic-concurrency retry *inside* the actor for persistence conflicts. That pattern is correct; this ADR applies the same shape *outside* the actor, for reaching the actor.

## Decision

Layer retry in the dispatch path with proper error classification. Distinguish transient-reach failures from permanent actor failures. Preserve idempotency across retries.

### Sub-Decision 1: Error taxonomy on `ActorError`

Add two methods to `ActorError` (no new variants):

```rust
impl ActorError {
    /// Retrying may succeed because the cause is timing or capacity, not logic.
    pub fn is_transient(&self) -> bool {
        matches!(self, ActorError::AskTimeout(_) | ActorError::MailboxFull)
    }

    /// Retrying is pointless because the actor is dead or the call is malformed.
    pub fn is_permanent(&self) -> bool {
        matches!(
            self,
            ActorError::Stopped
                | ActorError::SendFailed
                | ActorError::Panicked
                | ActorError::InitFailed
                | ActorError::MaxRestartsExceeded
                | ActorError::Custom(_)
        )
    }
}
```

**Why this approach**: Everyone who handles `ActorError` today re-derives this distinction by pattern match or string comparison on error messages. Encoding it once on the type makes retry policy mechanical, testable, and impossible to drift between call sites.

### Sub-Decision 2: Reshape `DispatchError`

Replace the single `DispatchError::ActorFailed(String)` with a classified enum:

```rust
pub enum DispatchError {
    /// Actor is reachable but the call never completed in time. Caller may retry.
    Transient { source: ActorError, attempts: u32 },
    /// Actor is broken or the call is logically invalid. Do not retry.
    Permanent { source: ActorError },
    /// Admission control or upstream backpressure. Caller should back off.
    /// (Introduced by ADR-0051 but reserved here so the enum is shaped once.)
    Deferred { retry_after_ms: u64 },
    // ... existing non-actor variants retained unchanged
}
```

HTTP layer (`odata/bindings.rs`) maps:
- `Transient` with exhausted budget → 503 + `Retry-After: 1`
- `Permanent` → 500
- `Deferred` → 503 + `Retry-After: {retry_after_ms/1000}`

**Why this approach**: HTTP semantics should reflect the underlying condition. 500 for a timing blip teaches bad habits; 503 with Retry-After lets intermediaries (reverse proxies, SDK clients) behave correctly.

### Sub-Decision 3: `retry::ask_with_backoff`

New module `/crates/temper-server/src/state/dispatch/retry.rs`:

```rust
pub struct RetryPolicy {
    pub total_deadline: Duration,           // TEMPER_DISPATCH_TOTAL_TIMEOUT_SECS, default 30s
    pub per_attempt_timeout: Duration,      // TEMPER_ACTION_TIMEOUT_SECS, default 5s
    pub max_attempts: u32,                  // TEMPER_DISPATCH_RETRY_MAX_ATTEMPTS, default 3
    pub base_delays_ms: [u64; 4],           // [0, 50, 200, 800]
    pub jitter: JitterMode,                 // Full jitter by default
}

pub async fn ask_with_backoff<M, R, F>(
    actor_ref: &ActorRef<M>,
    make_msg: F,                            // closure so each attempt builds a fresh envelope
    policy: &RetryPolicy,
    metrics: &MetricsCollector,
    tags: MetricTags<'_>,
) -> Result<R, DispatchError>
```

Semantics:
- Attempt N starts at `base_delays_ms[N] + jitter(0..base_delays_ms[N])`, capped so we never exceed `total_deadline`.
- `is_permanent()` short-circuits — no further attempts, return `Permanent`.
- `is_transient()` consumes an attempt and waits before retrying.
- Attempt budget OR deadline exhausted → `Transient { attempts }`.

**Why jitter**: full jitter (not equal jitter) avoids thundering-herd when many callers are waiting on the same actor. Matches AWS "Exponential Backoff And Jitter" recommendation.

### Sub-Decision 4: Apply to every ask site in the dispatch path

Seven call sites in `state/dispatch/actions.rs:235-243` and `state/entity_ops.rs:{455,476,549,599,684,753}`. All wrap through `retry::ask_with_backoff`. No call site has a bespoke retry.

**Why every site**: the goal is *invariant* resilience. Any site that escapes the wrapper becomes the next Railway 500.

### Sub-Decision 5: Idempotency with monotonic attempt id

Existing `IdempotencyCache` (`idempotency.rs`, 1h TTL, 1000-entry per-actor budget) already dedups HTTP-level client retries. Extend for dispatch-internal retries:

- Add a monotonic `attempt_seq: u64` per logical call (generated at dispatch entry, not per retry).
- `EntityMsg::Action` carries `attempt_seq`.
- Actor records "last `attempt_seq` I processed" per `idempotency_key`; if a stale attempt lands after a newer one succeeded, the actor returns the cached response without re-executing.

**Why**: in theory an ask could time out on the caller while the actor eventually processes it. Without sequence tracking, retry could double-execute on actors that buffer messages during slow persistence.

## Rollout Plan

1. **Phase 0 (ADR-approved)** — Add `is_transient`/`is_permanent` methods on `ActorError`. No-op on behavior.
2. **Phase 1 (Shipped, flag-off)** — Reshape `DispatchError`. Callers updated. `TEMPER_DISPATCH_RETRY_MAX_ATTEMPTS=1` keeps behavior identical to today.
3. **Phase 2 (Canary)** — Flip `TEMPER_DISPATCH_RETRY_MAX_ATTEMPTS=3` on a single tenant. Watch `temper_dispatch_ask_outcome_total{outcome:transient_retried_ok}` vs. `{transient_exhausted}`.
4. **Phase 3 (Production)** — Enable broadly. Decommission call-site manual retries (e.g., Katagami's `"Retrying after actor timeout"` logic).

## Readiness Gates

- `temper_dispatch_ask_outcome_total{outcome:transient_exhausted}` stays under 0.1% of traffic for 7 days.
- Zero new production 500s with "ask timeout" in the message.
- Datadog monitor `[OpenPaw] Dispatch Retry Budget Exhausted` is green.

## Consequences

### Positive
- Single transient blip in one actor stops cascading to user 500s.
- Error handling in WASM/SDK becomes uniform: `if err.is_transient(): retry; else: fail`.
- HTTP semantics (503 vs 500) correctly signal retriability to clients and proxies.

### Negative
- One additional layer on the hot path. Cost: one extra branch per call when retries are off; bounded sleeps + attempts when on.
- Error message changes. Downstream log parsers that match `"actor dispatch failed"` need updating.

### Risks
- **Retry amplification under actor outage.** If an actor is genuinely stuck, retries multiply load. Mitigation: total-deadline budget caps amplification at ~6x (3 attempts over 30s vs. one over 5s).
- **Idempotency cache memory.** More attempt sequences in the cache. Mitigation: existing per-actor 1000-entry budget already enforced.

### DST Compliance
- Retry delays drawn from `sim_now()` derived deterministic jitter, not `thread_rng`. `// determinism-ok: jitter seeded from sim_now() + attempt_seq`.
- `tokio::time::sleep` under DST is replaced by `SimScheduler::send_at`, preserving determinism.

## Non-Goals

- Adaptive retry budgets based on downstream health (future work).
- Circuit-breaker-style "stop trying this actor for N seconds" behavior.
- Per-action policy overrides (one global policy in v1).

## Alternatives Considered

1. **Call-site retry (status quo done right)** — Rejected. Proven to drift: production already shows `"Retrying after actor timeout"` in one place and not in seven others. Re-implementing per site guarantees future gaps.
2. **Block on mailbox instead of `try_send`** — Rejected. Violates TigerStyle bounded-queue invariant and trades `MailboxFull` for unbounded memory.
3. **Only extend the per-attempt timeout (e.g., 20s)** — Rejected as sole fix. Masks head-of-line blocking as "slower p99" instead of explicit backpressure. Also doesn't address `MailboxFull`.

## Rollback Policy

Set `TEMPER_DISPATCH_RETRY_MAX_ATTEMPTS=1`. Behavior returns to today's: first transient failure becomes the caller's problem. No data format changes; no schema changes; no rollback of persistent state required.
