# ADR-0094: Native File Blob Read Key Order

- Status: Proposed
- Date: 2026-05-17
- Deciders: Temper core maintainers
- Related:
  - ADR-0088: Native File `$value` Write Fast Path
  - ADR-0092: Bounded Background File Reactions
  - ADR-0093: Native Blob Transport Observability
  - `crates/temper-server/src/state/file_read_blobs.rs`
  - `crates/temper-server/src/state/file_writes.rs`
  - `os-apps/temper-fs/wasm/blob_adapter/src/lib.rs`

## Context

The native File `$value` write path introduced by ADR-0088 writes content to
the native blob store under the bucket-prefixed key
`temper-fs/{content_hash}`. The older WASM blob adapter writes external R2/S3
objects under `{content_hash}` because the provider bucket is already part of
the external endpoint configuration.

The File state currently stores only `content_hash`, not the exact storage key
that was used by the writer. To preserve compatibility after the native writer
landed, reads use a two-key fallback for external blob endpoints. Production
Datadog proof from the latency program showed that the order is now backwards
for freshly written native files: every sampled read first records a
`blob.transport.get` miss for `{content_hash}`, then a successful
`blob.transport.get` for `temper-fs/{content_hash}`. That extra object-store
round trip is directly user-visible on reads and on flows that validate or
serve freshly written File content.

We need to remove the wasted miss for the current hot path without breaking old
File content written by the WASM adapter.

## Decision

Prefer the native File blob key on read and keep the legacy external key as a
fallback.

### Sub-Decision 1: Native Key First

For all File content reads, try `temper-fs/{content_hash}` first.

**Why this approach**: The production writer now stores File bytes at the
native bucket-prefixed key. Trying that key first removes one remote object-store
miss from the current path while preserving the same File state and projection
contract.

### Sub-Decision 2: Legacy External Fallback Second

When the configured blob endpoint is an external provider endpoint, fall back to
`{content_hash}` if the native key is not found.

**Why this approach**: Existing files written through the WASM blob adapter may
still live at the legacy external key. A fallback keeps those files readable
without a data migration.

### Sub-Decision 3: No Spec or File Metadata Change in This Slice

Do not add `blob_key`, `storage_key`, or writer-version fields to the File IOA
spec in this PR.

**Why this approach**: Recording the exact key would remove ambiguity
permanently, but it is a cross-spec/schema/evolution change that needs a
separate migration and verification plan. The current measured regression has a
safe local fix that does not alter File semantics.

## Rollout Plan

1. **Phase 0 (Immediate)** - Change the File blob read helper to return an
   ordered key list, add focused unit coverage for native-first and legacy
   fallback ordering, and validate `temper-server` locally.
2. **Phase 1 (Rollout)** - Merge into Temper, bump TemperPaw, and deploy the
   new server build.
3. **Phase 2 (Production proof)** - Rerun the File `$value` live proof and
   query Datadog for `blob.transport.get` outcomes. Fresh native content should
   produce successful `temper-fs/{content_hash}` gets without a preceding
   `{content_hash}` 404.
4. **Phase 3 (Follow-up decision)** - Decide whether File state should store an
   explicit blob key or storage writer version so future migrations can avoid
   read-time probing entirely.

## Readiness Gates

- Focused `temper-server` tests prove external reads prefer the native key and
  retain legacy fallback ordering.
- Existing File value fast-path tests still pass.
- No tenant, URL, blob key, hash, or file name is added to metric tags.
- Production Datadog traces for fresh native File reads show no legacy-key 404
  before the successful native-key `blob.transport.get`.
- Legacy WASM-written external objects remain readable through fallback.

## Consequences

### Positive

- Removes one remote object-store miss from fresh native File reads.
- Keeps legacy external objects readable without a migration.
- Preserves the existing File state, projection, OData, and `$value` contracts.
- Gives the latency program an immediate measured improvement target before
  considering larger direct-upload or data-plane redesigns.

### Negative

- Legacy external objects that exist only at `{content_hash}` will now pay one
  native-key miss before fallback.
- The read path still has key ambiguity because File metadata does not record
  the exact blob key.

### Risks

- **Old external content read latency**: mitigated by fallback correctness and
  by the fact that the current hot writer is native.
- **Future key shapes**: mitigated by centralizing read key ordering in one
  helper rather than scattering conditional logic.
- **False production confidence**: mitigated by a Datadog proof gate that checks
  actual `blob.transport.get` outcomes after deployment.

### DST Compliance

- The change touches `temper-server`, a simulation-visible crate.
- The key order is deterministic and depends only on the configured endpoint
  type and content hash.
- No wall-clock time, randomness, filesystem ordering, thread scheduling, or
  network timing participates in simulation-visible decisions.
- No `// determinism-ok` annotations are required.

## Non-Goals

- No File IOA or CSDL schema change.
- No blob migration or object rewrite.
- No direct browser-to-object-store upload.
- No async acknowledgement before durable blob storage.
- No retry, backoff, or object-store client configuration change.

## Alternatives Considered

1. **Keep legacy-first ordering** - Rejected because production traces show it
   causes a predictable remote miss for freshly written native content.
2. **Remove legacy fallback** - Rejected because existing WASM-written external
   objects could become unreadable.
3. **Add `blob_key` to File state immediately** - Deferred because it requires
   spec, projection, migration, and compatibility work beyond this measured
   latency slice.
4. **Run a one-time migration first** - Deferred because the immediate user
   pain is in fresh native content and the fallback preserves correctness while
   avoiding migration risk.

## Rollback Policy

Restore the previous legacy-first helper order. No data migration is needed
because this ADR does not change stored File state or object contents.
