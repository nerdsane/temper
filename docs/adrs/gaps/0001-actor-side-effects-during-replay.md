# GAP-0001: Actor Side Effects During Event Sourcing Replay

## Status

Open

## Summary

Temper actors are designed as purely functional state machines — they receive
messages, compute state transitions, and emit events. This purity is what makes
event sourcing and replay safe: when an actor is rehydrated from its event log
(e.g. no running instance exists and one must be spawned from the database), we
simply replay the stored events through the handler and arrive at the correct
current state.

The problem arises when an actor's handler interacts with external systems
(HTTP calls, database writes, queue publishes, notifications, etc.). During
replay, those side effects would be **re-executed**, leading to:

- Duplicate API calls to third-party services
- Double-charging, double-booking, or duplicate notifications
- Corrupted external state that diverges from the actor's logical state
- Violations of exactly-once semantics that users implicitly expect

## How It Can Happen Today

The `Actor` trait's `handle` method is an async function with no constraint
preventing I/O:

```rust
async fn handle(
    &self,
    msg: Self::Msg,
    state: &mut Self::State,
    ctx: &mut ActorContext<Self>,
) -> Result<(), ActorError>;
```

Nothing in the type system or runtime prevents a developer from calling an
external HTTP API, writing to a database, or performing any other side-effecting
operation inside `handle`. During normal operation this works fine. During replay
it is a footgun.

## Impact

- **Severity**: High — silent data corruption or duplicate external mutations
- **Likelihood**: Moderate — any actor connected to an external system is
  vulnerable, and the failure mode is non-obvious (it only manifests on replay,
  not during normal operation)
- **Detection difficulty**: High — side effects during replay may succeed
  silently, making the problem hard to diagnose

## Possible Mitigations

### 1. Replay-aware context flag

Expose a `ctx.is_replaying()` flag so handlers can guard side effects:

```rust
if !ctx.is_replaying() {
    http_client.post(...).await?;
}
```

**Pros**: Simple, minimal API surface.
**Cons**: Relies on developer discipline; easy to forget.

### 2. Effect separation (command/event split)

Separate the handler into a pure state transition and an effect phase:

- `handle` returns state changes + a list of **effects** (commands to execute)
- The runtime executes effects only on first processing, not during replay

```rust
fn handle(&self, msg: Msg, state: &State) -> (StateChange, Vec<Effect>);
```

**Pros**: Side effects are structurally impossible during replay.
**Cons**: Larger API change; effects need a registry/executor.

### 3. Outbox pattern

Side effects are never executed inline. Instead, the handler writes an **outbox
record** as part of the event. A separate process reads the outbox and executes
effects idempotently. During replay, outbox records are re-created but the
executor deduplicates them.

**Pros**: Well-established pattern; naturally idempotent.
**Cons**: Adds infrastructure complexity (outbox table, executor, dedup logic).

### 4. Driver I/O recording (Temporal-style activity replay)

Treat all external interactions as **driver calls** routed through the runtime.
During first execution, the runtime records the inputs and outputs of every
driver invocation (HTTP request/response, DB query/result, etc.) into the event
log alongside the actor's state events. During replay, the runtime intercepts
driver calls and returns the **recorded output** instead of executing the real
call.

Actor-to-actor messages are already deterministic (they flow through the mailbox
system), so they can be assumed/replayed as-is. Only driver calls — the boundary
between the actor world and the external world — need recording.

This is analogous to how Temporal records activity inputs/outputs so that
workflow replay never re-executes activities, and how Microsoft Orleans journals
grain calls to external services.

```
First execution:          Replay:
  handler                   handler
    │                         │
    ├─ driver.call(req) ──►   ├─ driver.call(req) ──► runtime intercepts
    │  ◄── real response      │  ◄── recorded response (from event log)
    │                         │
    ├─ tell(other_actor) ──►  ├─ tell(other_actor) ──► replayed from log
    │                         │
    └─ state transition       └─ state transition (deterministic)
```

**Pros**: Fully transparent to the handler — no discipline required, no API
change. Side effects are structurally impossible during replay. Works for any
I/O, not just known patterns. Composable with the existing event log.
**Cons**: Requires all external I/O to go through a driver abstraction (no
raw `reqwest` or `tokio::net` in handlers). Recorded payloads increase event
log size. Schema evolution of recorded responses needs consideration.

### 5. Lint / static analysis guard

A compile-time or CI check that scans `handle` implementations for known I/O
patterns (HTTP clients, database pools, file system access) and flags them.

**Pros**: Catches violations early.
**Cons**: Heuristic-based; cannot catch all indirect I/O.

## Open Questions

- Which mitigation (or combination) fits Temper's philosophy best?
- Should the runtime enforce purity, or is documentation + linting sufficient?
- How does this interact with the MCP-based external API call model from
  ADR-0007 (governed external API calls)?
- Does the `ActorContext` need a formal "effect" submission API?
- For driver I/O recording (option 4): what is the right granularity of
  recording — per-request, per-driver, or per-handler invocation?
- How do we handle non-deterministic driver responses during replay when the
  external schema has changed (e.g. a field was added to an API response)?
- Can actor-to-actor messages be assumed deterministic in all cases, or are
  there edge cases (e.g. timer-triggered messages, external event ingestion)
  that also need recording?

## References

- [Event Sourcing — Martin Fowler](https://martinfowler.com/eaaDev/EventSourcing.html)
- ADR-0007: Governed External API Calls Through MCP
- ADR-0012: OAuth2 Enablement, Webhooks, Timers, Secret Templates
- Akka's `Effect` API in Akka Persistence Typed
- Microsoft Orleans' grain journaling and side-effect guidance
- [Temporal: How Workflows Execute Activities](https://docs.temporal.io/activities)
- [Temporal: Replay and Determinism Constraints](https://docs.temporal.io/workflows#deterministic-constraints)
