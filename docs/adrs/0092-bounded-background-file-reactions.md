# ADR-0092: Bounded Background File Reactions

- Status: Proposed
- Date: 2026-05-16
- Deciders: Temper core maintainers
- Related:
  - ADR-0081: Latency observability acceleration program
  - ADR-0083: Trace Budget and Fanout Summarization
  - ADR-0088: Native File `$value` Write Fast Path
  - ADR-0091: Query Projection Diff Index Upserts
  - `crates/temper-server/src/state/file_writes.rs`
  - `crates/temper-server/src/state/dispatch/actions.rs`
  - `crates/temper-server/src/trigger/dispatcher.rs`
  - `os-apps/temper-fs/specs/file.ioa.toml`

## Context

PERF-002 moved File projection upserts into the measured 42-91 ms p95 range for
the current production proof bucket. The remaining current-version Datadog
tail after long-lived event streams is File byte transfer:

- `PUT $value` internal p95 is about 611 ms and HTTP File `$value` p95 is about
  490 ms in the last two-hour current-version window.
- `GET $value` p95 is about 297 ms.
- `File.StreamUpdated` p95 is about 248 ms.
- `reaction.dispatch` p95 is about 220 ms.
- Cedar candidate phases are now usually sub-millisecond to low-single-digit
  milliseconds, and dispatch p95 is about 20-25 ms.

ADR-0088 already removed the WASM blob adapter from the built-in File write
path. The current native File write still waits for two kinds of work before
returning the HTTP `204`:

1. synchronous correctness work: hash bytes, durably write the blob, and commit
   `File.StreamUpdated` through the verified transition table;
2. cascade work: create the `FileVersion`, supersede the previous version, and
   increment Workspace usage through inline `[[action.triggers]]`.

The reaction dispatcher documentation already says reactions are
fire-and-forget and that the source transition is committed regardless of
reaction outcome. In practice, `dispatch_tenant_action` awaits the whole
reaction cascade inline. That is safer for historical tests, but it makes the
user-visible File upload wait on post-commit bookkeeping that can complete
immediately after the response without weakening the File state machine.

## Decision

Introduce an explicit dispatch option for background reaction execution and use
it only for the native File `$value` write path in the first PR.

### Sub-Decision 1: Keep the File Commit Synchronous

The File `$value` response may return only after all of the following are true:

- the request body is hashed;
- the content-addressed blob write succeeds;
- `File.StreamUpdated` commits through the verified transition table;
- the File entity state reflects `Ready`, `has_content`, `content_hash`,
  `mime_type`, `size_bytes`, and version counters.

**Why this approach**: These are the read-after-write and correctness boundaries
the user observes directly. The optimization must not acknowledge bytes before
Temper can read them back through the File entity.

### Sub-Decision 2: Run File Reactions in a Bounded Background Lane

For native File `$value` writes, run the post-commit inline trigger cascade in a
background task guarded by a process-wide semaphore budget. If the background
budget is saturated, fall back to the existing inline await path.

**Why this approach**: This removes reaction fanout from the common File upload
response path while preserving correctness under overload. The bounded fallback
means the system slows down instead of dropping FileVersion or Workspace
updates.

### Sub-Decision 3: Preserve Default Inline Reactions Elsewhere

Generic `dispatch_tenant_action` and all existing OData action dispatches will
continue to await reactions unless a caller explicitly opts into background
reactions.

**Why this approach**: The first latency slice should target the measured File
path without silently changing every generated app workflow. A broader reaction
execution policy can follow after production proof.

### Sub-Decision 4: Make the Choice Visible

The background path must emit a span such as
`reaction.dispatch.background` with the source entity/action and budget outcome.
The fallback path should log when the background budget is exhausted and it has
to await reactions inline.

**Why this approach**: The latency program needs evidence that response latency
fell because reaction work moved off the synchronous path, not because reaction
work disappeared.

## Rollout Plan

1. **Phase 0 (Immediate)** - Add the dispatch option, bounded background
   reaction helper, and native File `$value` opt-in. Update focused tests so the
   File response is fast-path correct immediately and FileVersion/Workspace
   effects are verified eventually.
2. **Phase 1 (Production proof)** - Roll into TemperPaw, deploy, rerun the
   File proof, and compare Datadog `PUT $value`, `File.StreamUpdated`,
   `reaction.dispatch`, FileVersion creation, and DB projection metrics.
3. **Phase 2 (Follow-up)** - If this is stable, consider making background
   reactions a spec-level or action-level policy with stronger queue metrics and
   replay repair.

## Readiness Gates

- Native File `$value` tests pass and still prove blob hash equality.
- FileVersion and Workspace trigger effects are observed eventually in tests.
- Background budget saturation falls back to inline execution rather than
  dropping reactions.
- No decision cache, Cedar bypass, direct projection write, or File state
  mutation outside `StreamUpdated` is introduced.
- Production proof shows File bytes read back immediately and FileVersion rows
  appear after the background cascade.

## Consequences

### Positive

- Removes measured reaction fanout from the common File upload response path.
- Keeps the File entity's verified transition and read-after-write semantics
  synchronous.
- Gives the platform an explicit dispatch policy knob instead of relying on
  misleading "fire-and-forget" comments.
- Provides a bounded overload behavior: inline await instead of silent loss.

### Negative

- FileVersion and Workspace side effects become eventually consistent for this
  HTTP response.
- Tests that assume inline reaction completion for File upload must wait for the
  side effect they care about.
- The dispatch API gains one more option.

### Risks

- **Lost background reaction**: mitigated by acquiring a bounded permit before
  spawning and falling back to inline reactions when saturated.
- **Read-after-write misunderstanding**: mitigated by keeping File state and
  blob content synchronous, and documenting that FileVersion is eventual on this
  path.
- **Process crash after File commit but before reaction completion**: this risk
  already exists conceptually for fire-and-forget reactions, but the current
  inline implementation reduces it. The first PR accepts this only for File
  uploads; Phase 2 should add durable reaction outbox/replay if the broader
  architecture moves this way.

### DST Compliance

- The change touches `temper-server`, a simulation-visible crate.
- No simulation-visible state depends on wall-clock time or random order.
- The background task is production-only post-commit side-effect execution and
  must carry a `// determinism-ok` annotation.
- The bounded semaphore is a production admission budget, not model state.

## Non-Goals

- No direct browser-to-object-store upload in this ADR.
- No removal of FileVersion or Workspace triggers.
- No change to generated app reaction semantics by default.
- No durable reaction outbox in the first PR.
- No projection cache or direct projection mutation.

## Alternatives Considered

1. **Keep awaiting reactions inline** - Safest, but leaves a measured 220-250 ms
   post-commit cascade in the File upload response path.
2. **Make all reactions background by default** - Architecturally coherent with
   the comments, but too broad for a measured first slice.
3. **Drop FileVersion creation from upload** - Rejected because version history
   is part of TemperFS correctness and user trust.
4. **Use unbounded `tokio::spawn` for reactions** - Rejected because the system
   needs a budgeted overload path.
5. **Build a durable reaction outbox first** - Stronger long-term architecture,
   but larger than needed to prove this latency slice. It remains the Phase 2
   path if background reactions become general.

## Rollback Policy

Set the native File `$value` dispatch call back to inline reaction execution.
Because File state and blob writes still commit through `StreamUpdated`, rollback
does not require data migration; it only changes whether future File upload
responses wait for the reaction cascade before returning.
