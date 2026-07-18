# ADR-0088: Native File `$value` Write Fast Path

- Status: Proposed
- Date: 2026-05-15
- Deciders: Temper core maintainers
- Related:
  - ADR-0170: Native immutable file read plane
  - ADR-0063: Object store for blob bytes
  - ADR-0081: Latency observability acceleration program
  - ADR-0083: Trace budget and fanout summarization
  - ADR-0084: Authz latency phase instrumentation
  - `crates/temper-server/src/odata/write.rs`
  - `crates/temper-server/src/state/mod.rs`
  - `crates/temper-server/src/blob_store.rs`
  - `os-apps/temper-fs/wasm/blob_adapter/src/lib.rs`

## Context

The production `PUT /tdata/Files('{id}')/$value` path is on the critical path for
TemperPaw file persistence. Datadog evidence from the 2026-05-15 production e2e
run shows a representative File `$value` upload at about 3.16 seconds. The
visible trace children show roughly 516 ms in `wasm.invoke`, roughly 220 ms in
the outbound blob `PUT`, roughly 355 ms in `File.StreamUpdated` reaction
dispatch, and repeated Cedar authorization spans around 70 ms in the reaction
fanout. The trace also shows a large pre-WASM gap that is not yet sufficiently
attributed.

The current `$value` write path has two distinct responsibilities coupled
together:

1. Durable byte storage for server-owned TemperFS File content.
2. Verified state transition and reaction dispatch through `File.StreamUpdated`.

The second responsibility is core to Temper's mission: specs, transition
tables, Cedar governance, audit, projections, and correctness must remain the
source of truth. The first responsibility is a platform data-plane operation.
For the built-in `File` entity, the server already has a native object-store
boundary (`ServerState::put_blob_object`) and a programmatic File upload helper
that computes the content hash and dispatches `StreamUpdated` without requiring
the WASM guest when the blob endpoint is local.

The generic WASM blob adapter remains useful: it is hot-reloadable, applies to
non-File media entities, and gives generated apps a uniform extension point.
For the built-in TemperFS File data plane, however, invoking WASM to perform an
object-store `PUT` adds avoidable latency and copy overhead while not changing
the verified entity behavior.

## Decision

Temper will introduce a native fast path for HTTP `PUT /tdata/Files('{id}')/$value`.
The path will:

1. Resolve and validate the OData parent and confirm the entity has
   `HasStream=true`.
2. Preserve the verification gate and existing request principal context.
3. Compute the SHA-256 content hash in the server.
4. Store the content at the existing content-addressed blob key
   `temper-fs/{content_hash}` through the native blob-store boundary, using a
   single content-addressed `PUT` for remote object stores rather than a
   `HEAD`-then-`PUT` write.
5. Dispatch the same `File.StreamUpdated` action with the same callback
   parameters the WASM blob adapter returns.
6. Return the same `204 No Content` response and `ETag` contract.

Only the byte-storage implementation changes for built-in `File` uploads. The
state transition, reaction cascade, Cedar checks, event sourcing, projections,
and generated app behavior continue to flow through the existing entity action
dispatch path.

### Sub-Decision 1: File-Only Native Path

The first implementation applies only when the resolved entity type is `File`.
Other `HasStream=true` entity types continue through the existing WASM
`blob_adapter`.

**Why this approach**: `File` is the observed production bottleneck and has a
stable platform-owned storage contract. Generalizing to every media entity
would turn a measured platform optimization into a semantic change for generated
apps.

### Sub-Decision 2: Preserve `StreamUpdated` as the Correctness Boundary

The native path must not mutate file state directly. It must dispatch
`File.StreamUpdated` and let the transition table, inline triggers, reactions,
projection pipeline, and Cedar policies do their normal work.

**Why this approach**: This keeps the optimization in the data plane rather than
the model plane. It avoids projection drift and keeps all versioning behavior
auditable through the same event and reaction chain as before.

### Sub-Decision 3: Keep WASM as Fallback and Extension Path

The generic path remains available for non-File media entities and as a fallback
if the native object-store boundary is not usable in an environment where the
blob adapter is installed.

**Why this approach**: The WASM blob adapter is still the flexible app-level
extension mechanism. This ADR does not remove hot-reloadable blob behavior; it
creates a fast platform path for a platform-owned entity where the semantics are
already known.

### Sub-Decision 4: Measure the Remaining Gap

The fast path must emit spans around native blob storage and action dispatch.
Follow-up instrumentation should attribute body-read/copy time for the OData
handler, because Datadog currently shows a large upload duration gap before the
visible blob `PUT`.

**Why this approach**: The program goal is not to make a single speculative
change. It is to iteratively remove validated latency while tightening
observability until every important millisecond has an owner.

## Rollout Plan

1. **Phase 0 (Immediate)** - Add the File-only native `$value` write fast path,
   keep the WASM path for other entity types, run local tests, and update the
   living latency dashboard with the implemented behavior and residual risks.
2. **Phase 1 (Production proof)** - Deploy the change, repeat the production
   TemperPaw file create/upload/read e2e, and compare Datadog traces against the
   2026-05-15 baseline: total upload latency, `wasm.invoke` absence/presence,
   blob-store duration, reaction fanout duration, and authz duration.
3. **Phase 2 (Next optimization)** - If the native fast path succeeds, address
   the next largest measured bucket: Cedar authorization cost in reaction fanout
   or OData body-read attribution, depending on the post-deploy trace.

## Readiness Gates

- Existing `File.StreamUpdated` tests continue to pass.
- A local or integration upload verifies file content can be written and read
  back with the expected SHA-256 hash.
- Production Datadog trace for a representative upload shows either no
  `wasm.invoke` on the File `$value` write path or an explicit fallback reason.
- Projection correctness evidence remains green: File status, `has_content`,
  `content_hash`, size, FileVersion creation, and read-back bytes match.
- Any fallback to WASM is visible in logs or spans.

## Consequences

### Positive

- Removes the WASM invocation from the hottest built-in File upload path when
  the native blob store can handle the request.
- Avoids an extra remote object-store existence check for new File writes.
- Keeps all verified entity semantics intact.
- Uses the same native blob-store boundary already used by read fast paths,
  published artifacts, and local blob handling.
- Gives Datadog a cleaner trace shape for the remaining latency buckets.

### Negative

- The server now owns one more built-in File data-plane path rather than routing
  all media writes through the generic blob adapter.
- If an environment relies on blob-adapter-specific upload behavior for File,
  the native path must fall back clearly or be disabled before rollout.

### Risks

- **Object-store behavior mismatch**: The native store may sign requests or check
  object existence differently from the WASM adapter. Mitigation: keep fallback
  and prove production with e2e read-back hash equality.
- **Version metadata mismatch**: The native path must compute
  `version_number`, `previous_version_id`, and `created_by` exactly as expected
  by the File reaction cascade. Mitigation: reuse `put_file_stream_content` and
  verify FileVersion/reaction outcomes.
- **Projection drift**: Avoided by dispatching `StreamUpdated` rather than
  writing projections directly.

### DST Compliance

- The change touches `temper-server`, a simulation-visible crate.
- No wall-clock time, random UUIDs, filesystem reads, network access, or
  spawned threads are introduced into simulation logic.
- Blob storage remains behind the existing production `BlobStore` boundary.
- Entity state changes still flow through deterministic transition-table
  dispatch.

## Non-Goals

- Replacing the generic WASM `blob_adapter`.
- Optimizing every `HasStream=true` entity type.
- Bypassing Cedar authorization, event sourcing, reactions, or projections.
- Solving Cedar reaction fanout latency in this ADR.
- Implementing true streaming upload all the way into the object store. That is
  a follow-up once the native File data plane is validated.

## Alternatives Considered

1. **Only add more instrumentation** - Useful, but insufficient: Datadog already
   shows the File upload path spends measurable time in an avoidable WASM
   invocation.
2. **Move all media entities to native storage immediately** - Too broad. It
   risks changing generated app semantics before we have evidence outside
   TemperFS File.
3. **Keep WASM but optimize guest code** - The guest `PUT` itself is not the
   dominant visible cost; the host/guest boundary and dispatch overhead are.
4. **Bypass `StreamUpdated` and update projections directly** - Rejected because
   it would compromise the verification, audit, reaction, and projection
   correctness boundaries that define Temper's architecture.

## Rollback Policy

Rollback is straightforward: route File `$value` writes back through the generic
`blob_adapter` path and leave the native helper available only for programmatic
local/internal writes. Because the fast path still dispatches `StreamUpdated`,
no data migration is required; rollback changes only future upload execution.
