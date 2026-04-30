# ADR-0072: ProgressMade Cadence

- Status: Accepted
- Date: 2026-04-29
- Deciders: Temper core maintainers
- Related:
  - ADR-0049: State Entry Timeouts and Durable Scheduler
  - ADR-0056: Durable State Timeouts and Silent-Exit Prevention
  - `os-apps/paw-agent/specs/session.ioa.toml`

## Context

`Heartbeat` proves a transport or tool loop is alive. It must not reset user-facing session phase budgets. `ProgressMade` proves meaningful forward motion and is the only signal that should reset `state_timeout` clocks.

The Postgres cutover review called out that the plan's ProgressMade cadence investigation was not recorded as its own ADR.

## Decision

Integrations that hold a `Session` in a long-running state emit `ProgressMade` when one of these occurs:

- Context preparation persists a new context entry or chunk.
- Provider streaming receives the first meaningful model chunk and then periodic meaningful chunks.
- Tool execution starts or completes a batch, writes a tool result, or checkpoints a resumable child session.
- Response application successfully appends to the session tree.

Integrations must not emit `ProgressMade` on passive waits, Discord typing indicators, keepalive frames, or gateway heartbeats. Those remain `Heartbeat` or transport-only signals.

Cadence rule: emit on each semantic milestone and at least every half of the current state's `after_seconds` budget while real work is happening. Do not emit faster than once every 5 seconds for the same `(tenant, session_id, state)` unless a distinct tool batch/result boundary occurred.

## Readiness Gates

- Session states that list `reset_on = ["ProgressMade"]` have at least one integration path that emits it during legitimate long work.
- Datadog `temper_state_timeout_reset_total{service:openpaw,state:Executing}` is nonzero during active tool execution.
- Datadog timeout firing does not increase after removing Turso write-gate band-aids.

## Consequences

### Positive

- State timeouts remain user-facing budgets rather than liveness checks.
- Long-running but productive sessions survive storage or provider tail latency.

### Negative

- Integrations must distinguish real work from keepalive noise.

### DST Compliance

`ProgressMade` is an IOA action. Its effects are replayable state transitions; no background watcher is introduced.

## Rollback Policy

Rollback is state-machine-local: remove `reset_on = ["ProgressMade"]` from a state and rely on fixed timeout fire behavior until the emitting integration is fixed.
