# ADR-0097: Overlap File Blob Write and State Read

- Status: Proposed
- Date: 2026-05-17
- Deciders: Temper core maintainers
- Related:
  - ADR-0063: Object store for blob bytes
  - ADR-0088: Native File `$value` write fast path
  - ADR-0092: Bounded background File reactions
  - ADR-0093: Native blob transport observability
  - ADR-0094: Native-first File blob read key order
  - `crates/temper-server/src/state/file_writes.rs`
  - `crates/temper-server/tests/file_value_fast_path.rs`

## Context

PERF-003B moved the projection residual out of the critical path. The next
current-version Datadog slice shows the largest user-visible non-streaming
resource is again the File data plane:

- `PUT $value` / `PUT /odata/{path}` p95 is about 378 ms in the 66e8 proof
  window.
- `state.put_file_stream_content.native` is also about 378 ms p95.
- `blob.transport.put_content` is about 220 ms p95.
- The remaining visible gap includes OData body buffering, File state lookup,
  content hashing, native blob transport, `StreamUpdated` dispatch, projection,
  and response assembly.

ADR-0088 already removed the old WASM blob-adapter write from built-in File
uploads. ADR-0092 already moved File reactions out of the HTTP response path.
The next safe local improvement is therefore not to weaken correctness, bypass
`StreamUpdated`, or claim direct-upload architecture prematurely. It is to stop
serializing two independent waits in the native File write path.

Today `put_file_stream_content_native` loads the File state first, then hashes
the body, then writes the content-addressed blob, then dispatches
`File.StreamUpdated`. After the content hash and object key are known, the File
state read and blob write are independent:

- the state read is needed for `version_number`, `previous_version_id`, and
  `created_by`;
- the blob write is needed before the verified `StreamUpdated` action can
  commit a `content_hash` that reads can trust.

An orphan content-addressed blob is acceptable if a later state read or action
dispatch fails; ADR-0063 already treats content-addressed orphan blobs as
harmless and deduplicated.

## Decision

For the built-in File native `$value` path, compute the content hash first, then
overlap the File state lookup with the content-addressed blob write. Dispatch
`File.StreamUpdated` only after both operations succeed.

### Preserve the Verified Commit Boundary

The blob write may run before the state read completes, but the File entity state
must not change until `File.StreamUpdated` is dispatched through the normal
verified transition path.

**Why this approach**: Reads and projections remain correct. A failed state read
or rejected action does not publish a new content hash in entity state.

### Keep Blob Durability Before State Commit

The HTTP request may return success only after the content-addressed blob write
succeeds and the verified File action succeeds.

**Why this approach**: This preserves read-after-write behavior for the existing
`$value` contract. Faster future designs such as direct upload sessions or
write-through local cache need a separate ADR because they change client-visible
semantics and recovery behavior.

### Keep Fallback Semantics

If the native blob write fails for an external blob endpoint, the existing
`blob_adapter` fallback remains available. State-read errors still fail the
request rather than falling back, matching existing behavior.

**Why this approach**: This keeps ADR-0088's safety valve for environments where
the native object-store path is not usable.

## Rollout Plan

1. **Phase 0 (Immediate)** - Refactor `put_file_stream_content_native` so the
   File state read and `put_content_addressed_blob` future are joined after the
   content hash is computed. Keep error classes and fallback behavior.
2. **Phase 1 (Proof)** - Run focused File `$value` tests and server checks. Roll
   into TemperPaw only if local gates pass.
3. **Phase 2 (Production measurement)** - Deploy and compare current-version
   `PUT $value`, `state.put_file_stream_content.native`, and
   `blob.transport.put_content` p95/p99. If the object-store leg remains the
   dominant floor, decide separately between direct upload sessions,
   write-through local cache with async remote replication, or accepting the
   provider latency for synchronous `$value`.

## Readiness Gates

- Existing `file_value_fast_path` tests pass.
- `cargo check -p temper-server` passes.
- `cargo fmt --all -- --check` and `git diff --check` pass.
- DST review confirms the change does not introduce spawned work or nondeterminism
  into simulated actor execution.
- Production proof preserves FileVersion correctness, exact readback, projection
  rows, and no WASM fallback unless explicitly logged.

## Consequences

### Positive

- Removes avoidable serial wait between state lookup and blob transport.
- Preserves the File spec, Cedar, event sourcing, projection, and FileVersion
  correctness boundaries.
- Keeps this slice small enough to deploy and measure before larger data-plane
  architecture changes.

### Negative

- If File state lookup is tiny relative to remote object-store latency, the
  improvement will be modest.
- A failed state read can leave an orphaned content-addressed blob that no File
  state references. This is already an accepted content-addressed storage tradeoff.

### Risks

- **Error precedence changes**: joining futures can observe both state and blob
  failures. Mitigation: return state errors first, preserving the old behavior
  where state was checked before blob commit.
- **Fallback regression**: native blob failures for external endpoints must still
  fall back to `blob_adapter`. Mitigation: keep `FileStreamContentError::BlobStore`
  unchanged.
- **Overstated win**: the remote blob provider may remain the floor. Mitigation:
  gate rollout claims on Datadog `PUT $value`, native state span, and
  `blob.transport.put_content` measurements.

### DST Compliance

This change touches `temper-server`, a simulation-visible crate. It does not
spawn tasks, threads, or background work. `tokio::join!` polls two existing
futures in the same task and does not introduce a new scheduler source. Entity
state mutation still occurs only through deterministic transition-table dispatch.
Blob I/O remains behind the existing production blob-store boundary.

## Non-Goals

- No direct browser-to-object-store upload sessions.
- No write-through local cache or asynchronous remote replication.
- No change to `File.StreamUpdated`, FileVersion generation, projections, or
  OData response semantics.
- No optimization for non-File `HasStream=true` entities.
- No change to the generic WASM `blob_adapter`.

## Alternatives Considered

1. **Direct upload sessions** - Potentially much faster for large files, but it
   changes the client contract, requires pre-signed URLs or multipart state,
   needs content-hash verification, and needs abandoned-upload cleanup.
2. **Return after local cache and replicate remotely later** - Attractive for
   perceived latency, but read-after-write and disaster recovery semantics change.
   This needs a dedicated correctness design.
3. **Bypass `StreamUpdated` and update projections directly** - Rejected because
   it breaks Temper's verified-state mission.
4. **Only add more spans** - Useful, but the state read and blob write are known
   independent waits, so a small safe overlap is justified before another
   measurement-only slice.

## Rollback Policy

Revert `put_file_stream_content_native` to the ADR-0088 serial order: load File
state, compute hash, write blob, dispatch `StreamUpdated`. Existing data remains
valid because object keys, File state, and FileVersion semantics do not change.
