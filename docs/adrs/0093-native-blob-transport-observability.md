# ADR-0093: Native Blob Transport Observability

- Status: Proposed
- Date: 2026-05-16
- Deciders: Temper core maintainers
- Related:
  - ADR-0081: Latency Observability Acceleration Program
  - ADR-0083: Trace Budget and Fanout Summarization
  - ADR-0088: Native File `$value` Write Fast Path
  - ADR-0092: Bounded Background File Reactions
  - `crates/temper-server/src/blob_store.rs`
  - `crates/temper-server/src/runtime_metrics.rs`
  - `crates/temper-wasm/src/metrics.rs`

## Context

PERF-005B moved the FileVersion/RecordVersion reaction cascade off the native
File `$value` response path. Production proof
`file-reaction-background-live-proof-20260516215205` showed that this worked:
sampled `PUT $value` p95 fell to about 238.7 ms, `File.StreamUpdated` p95 fell
to about 13.7 ms, and `reaction.dispatch.background` appears after the HTTP
response boundary.

The remaining user-visible File byte latency is now mostly inside
`state.put_file_stream_content.native`. Datadog can show the whole native span,
but it cannot currently explain the blob portion because the native
`BlobStore` only records `temper_blob_io_wait_duration_ms`, which is semaphore
queue wait. It does not record the actual local filesystem or S3/R2 transport
duration, request outcome, HTTP status class, or payload size. The older
`temper_blob_transport_*` metrics only cover WASM host HTTP blob requests, so
the dashboard has a misleading gap exactly where the latest production proof
needs evidence. Native metrics must use a distinct name family rather than
reusing the WASM request counter with a different tag shape.

Without native blob transport timing, the next optimization would be guesswork:
we cannot tell whether to focus on R2 endpoint behavior, request/client setup,
payload copying, File state loading, actor cold start, DB append shape, or a
larger direct-upload data plane.

## Decision

Add native blob transport spans and metrics at the `temper-server` `BlobStore`
boundary without changing File semantics.

### Sub-Decision 1: Keep the Existing Queue-Wait Metric

Continue to emit `temper_blob_io_wait_duration_ms` for time spent waiting on the
shared blob I/O semaphore.

**Why this approach**: Queue wait is still useful as a saturation signal. It
answers "are we waiting for our own backpressure budget?" but should no longer
be treated as total blob latency.

### Sub-Decision 2: Add Native Transport Duration and Request Counters

Emit a new histogram and counter from all native blob operations:

- `temper_blob_native_transport_duration_ms`
- `temper_blob_native_transport_requests_total`

The tag set is intentionally bounded:

- `operation`: `put`, `put_content`, `get`, or `head`
- `backend`: `local_fs` or `s3`
- `outcome`: `ok`, `not_found`, or `error`
- `status_code_class`: `2xx`, `4xx`, `5xx`, `none`, or `error`

**Why this approach**: These dimensions are enough to distinguish object-store
transport from local filesystem work, separate reads from writes, and group
errors without creating high-cardinality metric tags.

### Sub-Decision 3: Add Payload Size Histograms

Emit:

- `temper_blob_native_transport_request_bytes`
- `temper_blob_native_transport_response_bytes`

with the same bounded operation/backend/outcome/status tags.

**Why this approach**: File latency is byte-size sensitive. Size histograms let
the dashboard correlate transport latency with payload size without tagging raw
paths, hashes, or tenant IDs.

### Sub-Decision 4: Add Trace Spans Around the Transport Boundary

Create spans named:

- `blob.transport.put`
- `blob.transport.put_content`
- `blob.transport.get`
- `blob.transport.head`

Span fields include backend, operation, request bytes, response bytes, outcome,
status code, and duration. Blob keys should not be recorded as span fields.

**Why this approach**: APM traces should make the native File byte path
self-explaining while avoiding high-cardinality or sensitive object names.

### Sub-Decision 5: Do Not Change File Acknowledgement Semantics

The File `$value` response still waits for hash computation, durable blob write,
and verified `File.StreamUpdated` commit. This ADR only adds measurement.

**Why this approach**: The latency program is allowed to be bold, but
correctness remains a hard boundary. Direct upload, async upload, or local
write-through cache designs need separate ADRs after this measurement proves
where the remaining time goes.

## Rollout Plan

1. **Phase 0 (Immediate)** - Add the metrics/spans in Temper, unit-test the
   bounded metric labels, and validate `temper-server` locally.
2. **Phase 1 (Rollout)** - Bump TemperPaw to the merged Temper revision,
   update the Datadog metric contract/dashboard/monitors to include native blob
   transport duration and request counters, then deploy.
3. **Phase 2 (Production proof)** - Rerun the File `$value` proof and query
   Datadog for `PUT $value`, `state.put_file_stream_content.native`,
   `blob.transport.put_content`, `temper_blob_native_transport_duration_ms`,
   queue wait, request bytes, and response bytes.
4. **Phase 3 (Optimization decision)** - Decide whether the next slice is
   object-store endpoint/client tuning, read/write-through cache, streaming
   upload, presigned direct upload, or actor/DB work outside blob transport.

## Readiness Gates

- `cargo test -p temper-server` focused blob tests pass.
- `cargo check -p temper-server` and `cargo fmt --all -- --check` pass.
- New metrics use bounded tags and do not include tenants, blob keys, file
  names, hashes, URLs, or auth material.
- Production APM shows native `blob.transport.*` spans under File `$value`
  traces after rollout.
- Datadog can query p95/p99 for native blob transport duration by operation and
  backend.

## Consequences

### Positive

- The remaining native File byte-path latency becomes attributable.
- Existing queue-wait metrics keep their meaning while no longer pretending to
  cover remote object-store duration.
- Future data-plane changes can be chosen from production evidence.
- Payload-size correlation becomes possible without high-cardinality labels.

### Negative

- Adds a small amount of metric and span overhead on blob operations.
- Datadog dashboards and monitors need another contract update in TemperPaw.
- The first PR improves diagnosis, not user-visible latency by itself.

### Risks

- **Metric cardinality creep**: mitigated by a fixed tag set with no keys,
  URLs, tenants, hashes, or file names.
- **Trace volume**: mitigated by one span per native blob operation, aligned
  with existing sampled APM behavior.
- **Misreading local filesystem timings as production R2 timings**: mitigated
  by the `backend` tag.

### DST Compliance

- The change touches `temper-server`, a simulation-visible crate.
- Timing uses `std::time::Instant` only for production observability and must
  carry `// determinism-ok` annotations where new wall-clock measurement is
  introduced.
- No simulation-visible state, ordering, IDs, or transition decisions depend on
  wall-clock time or metric output.

## Non-Goals

- No direct browser-to-object-store upload.
- No async acknowledgement before the blob is durable.
- No object-store provider change.
- No retry/backoff policy change.
- No FileVersion or projection behavior change.
- No path, tenant, hash, or URL tags in Datadog metrics.

## Alternatives Considered

1. **Use only existing `temper_blob_io_wait_duration_ms`** - Rejected because it
   measures only semaphore wait and returned no useful samples for the current
   production proof.
2. **Instrument only APM spans** - Rejected because dashboards and monitors need
   percentile metrics independent of trace sampling.
3. **Instrument only metrics** - Rejected because trace shape is the fastest way
   to explain a single File proof request end to end.
4. **Start with direct upload architecture** - Rejected for this slice because
   the measured residual has not yet been decomposed enough to justify a
   correctness-affecting data-plane change.

## Rollback Policy

Remove the new metric fields, recording helper, and `blob.transport.*` spans.
No data migration is required because File state, blob storage, and projection
semantics are unchanged.
