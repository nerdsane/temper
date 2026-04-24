# Proof Report: Session Stall Remediation

## Date

2026-04-24

## Branch / Commit

- Branch: `codex/session-stall-remediation`
- Commit: PR head commit
- Companion OpenPaw branch: `codex/session-stall-remediation`

## What Was Done

- Moved live query projection maintenance off the dispatch success path.
- Added background projection metrics:
  - `temper_query_projection_update_enqueued_total`
  - `temper_query_projection_update_duration_ms`
  - `temper_query_projection_update_error_total`
- Kept those metrics in a small dedicated module so `runtime_metrics.rs` stays under the readability ratchet threshold.
- Wrapped background projection work in a `dispatch.phase.query_projection` span.
- Updated query projection integration tests to poll for eventual projection updates.
- Fixed guest metric kind handling so `kind = "count"` and `kind = "counter"` both produce OTEL counters.

## Verification Flow

| Step | Expected | Actual | Status |
|------|----------|--------|--------|
| Red test: projection mode | Test fails before background mode helper exists | `cargo test -p temper-server query_projection_updates_are_not_on_the_dispatch_critical_path` failed with missing helper/type | Pass |
| Red test: guest metric count kind | Test fails before count alias helper exists | `cargo test -p temper-wasm guest_metric_count_kind_is_counter` failed with missing helper | Pass |
| Projection mode unit test | Query projection mode is background | `cargo test -p temper-server query_projection_updates_are_not_on_the_dispatch_critical_path` passed | Pass |
| Query projection integration tests | Live upsert/delete and backfill remain correct under eventual updates | `cargo test -p temper-server --test query_projection_backfill` passed, 3 tests | Pass |
| Guest metric kind unit test | `count` and `counter` both classify as counters | `cargo test -p temper-wasm guest_metric_count_kind_is_counter` passed | Pass |
| Instrumentation guard | All registered runtime metrics have emission sites | `cargo run -p temper-server --bin check_instrumentation` passed | Pass |
| Readability ratchet | New metrics do not add a >1000-line production file | `bash scripts/readability-ratchet.sh check .ci/readability-baseline.env` passed | Pass |

## Verification Results

- Dispatch now enqueues durable query projection maintenance and returns without awaiting projection storage writes.
- Projection integration tests pass by observing eventual query index and catalog state.
- Projection update latency and error metrics have both registration and emission sites.
- Guest-emitted budget counters from OpenPaw will be interpreted as counters when they use the existing `kind = "count"` convention.

## What Worked

- Existing query projection coverage adapted cleanly to eventual consistency with bounded polling.
- The instrumentation checker accepted the new runtime metrics.
- The guest metric fix improves old and new WASM counter emissions without changing guest module call sites.

## What Didn't Work

- No issue in the focused Temper verification.

## Limitations

- This proof does not include a live OpenPaw source-search replay. It verifies the platform dispatch/projection behavior and metric plumbing that the OpenPaw remediation depends on.

## What Still Doesn't Work

- Query projections are now eventually consistent after live dispatch. This is intentional, but consumers that require read-your-write query-plane visibility must poll or read the entity directly.

## Artifacts

- `crates/temper-server/src/state/dispatch/effects.rs`
- `crates/temper-server/src/query_projection_metrics.rs`
- `crates/temper-server/src/runtime_metrics.rs`
- `crates/temper-server/tests/query_projection_backfill.rs`
- `crates/temper-wasm/src/host_trait.rs`

## Architecture Diagram

```text
entity dispatch succeeds
  -> state transition durable
  -> timers / reactions / callbacks continue
  -> query projection update enqueued
       -> background task upserts/removes query projection
       -> emits duration and error metrics
```
