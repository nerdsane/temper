# ADR-0182: Reuse the S3 blob client and retry a stalled GET

- Status: Accepted
- Date: 2026-10-24
- Deciders: Temper core maintainers
- Related:
  - ADR-0071: Storage Retry Classification
  - ADR-0093: Native Blob Transport Observability
  - `crates/temper-server/src/blob_store.rs`
  - `crates/temper-server/src/blob_store/s3_transport.rs`

## Context

On 2026-10-23 the native blob transport monitors for `service:temperpaw`,
`backend:s3`, `operation:get` flapped all day. The monitors named "wait"
query `temper_blob_native_transport_duration_ms`, not the semaphore
histogram. In the sampled spans, successful gets had a p95 around 1.5s and
a tail around 2.5s. Busy time on those spans was under 10ms; the rest was
socket idle, including reads of a few tens of kilobytes. Semaphore wait
stayed around 5ms. Cache hits (`outcome:cache`) stayed near 0ms.

Forty-three gets never recorded an outcome. Their durations sat in a 50ms
band from 7.94s to 7.99s, which is the caller giving up while the GET was
still idle. `BlobStore::s3` built a new `reqwest::Client` on every store,
and every read builds a new store, so there was no connection pool.
`connect_timeout` was 10s, longer than that cancellation, and a timed-out
handshake was not retried. The File `$value` response failed instead of
returning bytes that a second attempt would have read.

ADR-0071 keeps plain timeout text from being retried at the dispatch layer.
A retry of an idempotent GET belongs on the blob client, where a timeout is
a stalled socket rather than a saturated write gate.

## Decision

Share one process-wide S3 `reqwest::Client` across blob stores. Keep idle
pooled connections for 45s, send TCP and HTTP/2 keepalives, and bound
connect to 2s so a blackholed handshake fails inside one attempt.

A buffered GET may try twice inside a 7s budget. Each attempt is capped at
3s. A timeout, connect failure, or dropped body is retried. An HTTP status,
including 404 and 5xx, is not: S3 already answered. Puts and streaming
reads use the same client and do not gain this retry.

**Why this approach**: The idle-versus-busy split says the time is the
handshake, not the semaphore and not the cache. A shared pool removes that
handshake from the steady state. The 7s budget is inside the observed ~8s
cancellation, so a stalled first attempt can still succeed on the second
before the response is failed.

## Consequences

Steady-state gets reuse a live connection. A single dead socket no longer
consumes the whole response budget. A transport that stays stalled for both
attempts still returns an error, now before the caller cancels the read
with no outcome. PUT semantics are unchanged. The client is production I/O;
simulation state does not depend on it.
