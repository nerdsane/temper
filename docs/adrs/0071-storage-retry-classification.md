# ADR-0071: Storage Retry Classification

- Status: Accepted
- Date: 2026-04-29
- Deciders: Temper core maintainers
- Related:
  - ADR-0048: Dispatch Retry and Error Taxonomy
  - ADR-0068: Turso Write Gate Retrospective and Removal
  - `crates/temper-server/src/state/dispatch/mod.rs`

## Context

The April 28 incident was amplified because storage admission text containing `timed out` was classified as a transient write error. That caused retries to re-enter the same saturated Turso gate and stretched a single trajectory write into a user-visible session failure.

## Decision

Storage retry classification must be semantic and conservative:

- Optimistic concurrency conflicts may retry through the ADR-0048 catch-up path.
- Explicit connection reset, connection closed, unavailable, and serialization-retry signals may retry when the caller budget still has headroom.
- Plain `timeout`, `timed out`, and admission-timeout text is not retryable by substring match.
- Backend-specific retry policy belongs at the backend boundary; dispatch-level retry must not infer transient write safety from arbitrary error prose.

## Readiness Gates

- Unit coverage asserts that `timeout` and `timed out` are not transient write matches.
- Dispatch retry logs include the concrete error kind used for the decision.
- Datadog alerting watches retry amplification via `temper_dispatch_ask_attempts` and backend append latency.

## Consequences

### Positive

- Saturated storage gates fail fast instead of self-amplifying.
- The policy is portable across Turso and Postgres because it relies on explicit categories.

### Negative

- Some real transient backend stalls may fail instead of retrying until they are classified explicitly.

### DST Compliance

Retry classification is deterministic string/category handling and remains safe in simulation. Timing budgets are external and tested with fixed attempt budgets.

## Rollback Policy

Rollback would require restoring timeout substring matching, which is intentionally discouraged. Prefer adding a backend-specific explicit transient category with tests.
