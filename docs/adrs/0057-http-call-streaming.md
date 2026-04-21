# ADR-0057: `http_call_streaming` — bidirectional streaming HTTP for WASM integrations

- Status: Proposed
- Date: 2026-04-20
- Deciders: Temper core maintainers
- Related:
  - ADR-0032: host-connect-call (precedent for streaming host functions; this is the HTTP dual)
  - ADR-0056: HttpEndpoint (inbound router — depends on this ADR)
  - `crates/temper-wasm/src/host_trait.rs` (WasmHost trait)
  - `crates/temper-wasm/src/engine.rs` (host function linking)
  - `crates/temper-wasm-sdk/src/host.rs` (FFI bindings)

## Context

ADR-0032 added `connect_call` to support Connect-protocol server-streaming RPCs, but the framing is Connect-specific and the surface is outbound-only. For two concrete needs we now have no primitive that fits:

1. **Inbound streaming (ADR-0056 `HttpEndpoint` dispatch)** — the integration must pull the request body incrementally and push response bytes incrementally. Today's `WasmHost` HTTP surface is `(request_bytes) -> (status, response_bytes)`, strictly buffered. A `git push` of a 500 MiB pack would materialize the full body in WASM linear memory twice (request + response). Linear memory caps make this infeasible even at 100 MiB.

2. **Outbound streaming with arbitrary bodies** — downstream apps need to `git clone` from a peer temper-git, proxy S3 GETs, etc. `host_http_call` buffers both sides; `connect_call` imposes Connect framing that no third-party server speaks.

Both point at the same primitive: bidirectional byte streams, bounded resident memory, kernel-mediated backpressure.

## Decision

Add `http_call_streaming` to the `WasmHost` trait and expose it through the SDK as a pair of `Read` / `Write` adapters over host-managed bounded channels.

### Sub-Decision 1: Host-side interface

```rust
impl WasmHost {
    /// Initiate a streaming HTTP exchange.
    ///
    /// Returns opaque handles the guest can read/write via
    /// `host_http_stream_read_request` / `host_http_stream_write_response`
    /// (or their outbound duals). The kernel owns the underlying
    /// channels and enforces bounded buffering.
    async fn http_call_streaming(
        &self,
        direction: HttpStreamDirection,
        request: HttpRequestHead,
    ) -> Result<HttpStreamHandles, WasmHostError>;
}

pub enum HttpStreamDirection {
    /// Guest is serving an inbound request routed by HttpEndpoint.
    /// Guest reads request body, writes response body.
    Inbound,
    /// Guest is making an outbound call.
    /// Guest writes request body, reads response body.
    Outbound,
}

pub struct HttpStreamHandles {
    pub request_body: StreamHandle,   // directionality depends on direction
    pub response_body: StreamHandle,
}
```

### Sub-Decision 2: Kernel-mediated backpressure

Each `StreamHandle` is backed by a bounded MPSC channel (default capacity: 64 chunks × 16 KiB = 1 MiB per handle). When the channel is full, the sending side blocks in a way that is observable to the other side of the FFI boundary:

- Guest writes that would block return `Err(WouldBlock)`; the SDK's `Write` adapter loops through a cooperative `host_yield()` that suspends the Wasmtime fiber.
- Host reads that would block suspend the axum task; backpressure propagates to the TCP receive window.
- The aggregate cap (both directions combined) must not exceed 4 MiB of resident memory per active request. ADR-0056's Gate 2 (1 GiB round-trip under 64 MiB RSS) validates this.

### Sub-Decision 3: FFI surface

Four new imports in `env` (names chosen to match existing `host_*` convention):

| FFI | Purpose |
|---|---|
| `host_http_stream_read(handle, buf_ptr, buf_len) -> i32` | read up to `buf_len`; returns bytes read, 0 for EOF, -1 `WouldBlock`, -2 other error |
| `host_http_stream_write(handle, buf_ptr, buf_len) -> i32` | write up to `buf_len`; returns bytes written, -1 `WouldBlock`, -2 error |
| `host_http_stream_close(handle) -> i32` | half-close; response streams complete-with-success when closed cleanly |
| `host_http_stream_begin(direction, headers_ptr, headers_len, method_ptr, method_len, url_ptr, url_len, handles_ptr) -> i32` | open a streaming exchange; writes the two handle IDs into `handles_ptr` |

Head metadata (method, URL, headers, status, trailers) is still passed as small contiguous buffers — only the body is streamed. This keeps the FFI surface small and preserves the existing header-allocation path.

### Sub-Decision 4: SDK ergonomics

The SDK wraps the raw FFI in two idiomatic types:

```rust
pub struct HttpRequestBody { /* impl std::io::Read + AsyncRead */ }
pub struct HttpResponseBody { /* impl std::io::Write + AsyncWrite */ }

// Inbound (from HttpEndpoint):
#[temper_wasm_sdk::http_handler]
pub fn handle(ctx: HttpRequestContext) -> HttpResponse {
    let mut body = ctx.request_body();       // HttpRequestBody
    let mut out  = ctx.response_body();      // HttpResponseBody
    // copy with bounded buffer:
    std::io::copy(&mut body, &mut out).map_err(...)?;
    HttpResponse::streaming(200, /*trailers=*/ &[])
}
```

For outbound calls the guest constructs the request head and receives the two handles back. The SDK offers a `streaming_get(url, headers) -> (HttpResponseHead, HttpResponseBody)` shortcut for the common case.

### Sub-Decision 5: Cedar reuses existing `http_call` action for outbound; new `HandleHttp` action for inbound

- Outbound streaming calls authorize the same way `host_http_call` and `connect_call` do: `Action::"http_call"` with resource `HttpEndpoint::{domain}`. No new action — the kernel just dispatches the pre-authorized call through a different transport.
- Inbound dispatch (ADR-0056) runs the bound integration's policy with `Action::"HandleHttp"`. Cedar runs **before** byte one of the request body is handed to the guest, so a reject cleanly terminates the exchange with `403`.

### Sub-Decision 6: Timeout and cancellation

- `HttpEndpoint.TimeoutSecs` applies to the full inbound exchange (head received to response half-close). On expiry the kernel closes both handles; guest reads/writes return `-3 Aborted`.
- Outbound calls inherit the integration's default action timeout (ADR-0045 `wasm-default-timeout`) unless overridden per-call.
- Guest yields cooperatively; there is no preemption. Guests that ignore `Aborted` and spin get fiber-killed after a 5 s grace period (same mechanism as existing WASM timeouts).

## Rollout Plan

1. **Phase 0 (this PR)** — ADR only.
2. **Phase 1** — Implement `WasmHost::http_call_streaming` for the outbound case only, with a `streaming_download` SDK helper. One integration test: download a 100 MiB file from a local test server into a sha256 hasher, resident memory capped at 4 MiB.
3. **Phase 2** — Inbound case. Land alongside ADR-0056 Phase 2 (the two must ship together — ADR-0056's dispatcher needs this primitive). Echo-handler 1 GiB round-trip test.
4. **Phase 3** — DST coverage: a model that drives a random split of a large body across chunk boundaries, including mid-body errors and cancellations, asserting no-hang / no-leak invariants.

## Readiness Gates

- **Gate 1** — 1 GiB round-trip under 64 MiB resident memory (kernel + guest combined).
- **Gate 2** — Backpressure demonstrable: a slow consumer throttles the producer; no unbounded buffering anywhere.
- **Gate 3** — Cancellation is observable and clean: aborted streams free all handles within one tick of the executor; DST asserts no handle leaks across 10k random scenarios.
- **Gate 4** — ADR-0032 `connect_call` continues to work unchanged (it is a separate primitive; this ADR does not replace it).

## Alternatives considered

- **Add streaming to `host_http_call` by overloading its return type** — rejected. Breaks the existing buffered-semantics contract that ~a dozen integrations depend on.
- **Generalize `connect_call` to arbitrary framing** — rejected. Connect's 5-byte frame prefix is a Connect-specific detail; forcing it on plain HTTP servers would not interoperate.
- **Expose raw socket I/O to WASM** — rejected. Cedar's domain-level authorization cannot span it, and it couples the guest to TLS/HTTP concerns we deliberately keep in the host.
- **Let guests preallocate a large ring buffer and do their own backpressure** — rejected. Kernel-mediated bounded channels are simpler to audit and give DST a single place to inject faults.
