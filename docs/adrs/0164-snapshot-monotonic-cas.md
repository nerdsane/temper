# ADR-0164: Monotonic snapshot compare-and-set (ARN-239)

- Status: Accepted
- Date: 2026-07-12
- Related: ARN-239; all EventStore backends

## Decision

`save_snapshot` must:

1. Reject `sequence_nr < current_latest` with `ConcurrencyViolation`
2. Treat `sequence_nr == current` + identical bytes as idempotent success
3. Reject same-sequence conflicting content
4. Append history only (no overwrite of prior sequence content)

Applied to Sim, Postgres, Turso, and Redis backends.
