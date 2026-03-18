# ADR-0032: host_connect_call — Connect Protocol Support for WASM Modules

- Status: Accepted
- Date: 2026-03-17
- Deciders: Temper core maintainers
- Related:
  - ADR-0031: temper-native-agent (agent architecture)
  - `crates/temper-wasm/src/host_trait.rs` (WasmHost trait)
  - `crates/temper-wasm/src/engine.rs` (host function linking)
  - `crates/temper-wasm/src/authorized_host.rs` (Cedar authz gate)
  - `crates/temper-wasm-sdk/src/host.rs` (FFI bindings)
  - `crates/temper-wasm-sdk/src/context.rs` (SDK wrapper)

## Context

WASM modules in temper-agent need to execute processes in E2B cloud sandboxes. E2B's envd daemon exposes process execution via the **Connect protocol** — a protobuf RPC framework that works over HTTP/1.1 with JSON encoding.

The existing `host_http_call` returns `(status_code, response_body_string)` which works for standard REST APIs. However, Connect server-streaming RPCs use a **5-byte frame prefix** (1 flag byte + 4 length bytes) per message in the response body. The response is binary-framed even when using JSON encoding, making it incompatible with string-based `host_http_call`.

Specifically, E2B's `process.Process/Start` endpoint:
- URL: `POST https://{sandbox_url}/process.Process/Start`
- Request: JSON body `{"command": "ls", "envs": {}, "cwd": "/workspace"}`
- Response: Binary-framed stream of JSON messages, each with a 5-byte prefix
- Content-Type: `application/connect+proto` (but supports `application/connect+json`)

Without a host function that understands Connect framing, WASM modules cannot execute processes in E2B sandboxes.

## Decision

### Sub-Decision 1: Add `connect_call` to `WasmHost` trait

Add a new method to the `WasmHost` trait that handles Connect protocol server-streaming RPCs:

```rust
/// Make a Connect protocol server-streaming RPC call.
/// Returns a vec of decoded JSON message frames.
async fn connect_call(
    &self,
    url: &str,
    headers: &[(String, String)],
    body: &str,
) -> Result<Vec<String>, String>;
```

**Why this approach**: The host (Rust with full stdlib) can easily parse the 5-byte frame prefix and extract individual message payloads. WASM modules get clean JSON strings without needing to handle binary framing. This keeps WASM modules simple and avoids pulling protobuf/Connect libraries into the WASM target.

### Sub-Decision 2: Use JSON encoding, not binary protobuf

Connect protocol supports both protobuf and JSON content types. We use JSON:
- Request: `Content-Type: application/json`
- Response: Each frame contains a JSON object
- No protobuf compilation or `prost` dependency needed

**Why this approach**: JSON is already the lingua franca of temper WASM modules. The E2B envd daemon accepts JSON-encoded Connect requests. Adding protobuf would require `prost` in the WASM SDK, increasing binary size and complexity for no benefit.

### Sub-Decision 3: Frame parsing in the host

The host implementation:
1. Sends HTTP POST with `Content-Type: application/json` and `Connect-Protocol-Version: 1`
2. Reads the full response body as bytes
3. Parses frames: each frame is `[1 flag byte][4 big-endian length bytes][N payload bytes]`
4. Flag byte 0x00 = data frame (JSON payload), 0x02 = trailer frame (end-of-stream metadata)
5. Returns all data frame payloads as a `Vec<String>`

### Sub-Decision 4: Cedar authorization reuses `http_call` action

Connect calls go through the same `authorize_http_call` Cedar check. The action remains `Action::"http_call"` with resource `HttpEndpoint::{domain}`. No new Cedar action type needed.

**Why this approach**: From a security perspective, a Connect call is still an outbound HTTP call to a domain. The existing policy structure (`[integration.config]` declares allowed domains, Cedar governs which modules can call which domains) works unchanged.

### Sub-Decision 5: FFI binding as `host_connect_call`

New FFI function in the WASM SDK:

```rust
extern "C" {
    pub fn host_connect_call(
        url_ptr: i32, url_len: i32,
        headers_ptr: i32, headers_len: i32,
        body_ptr: i32, body_len: i32,
        result_buf_ptr: i32, result_buf_len: i32,
    ) -> i32;
}
```

Returns JSON-encoded `Vec<String>` (array of frame payloads) written to the result buffer. The SDK wrapper `Context::connect_call()` deserializes this into `Vec<String>`.

## Rollout Plan

1. **Phase 0 (This PR)**:
   - Add `connect_call` to `WasmHost` trait with default `Err("not supported")` impl
   - Implement in `ProductionWasmHost` using `reqwest` (read full body, parse frames)
   - Implement in `SimWasmHost` with canned responses
   - Add `host_connect_call` to wasmtime linker in `engine.rs`
   - Add `connect_call` FFI binding and SDK wrapper
   - Wire Cedar authz through existing `authorize_http_call`
   - Update `tool_runner` to use `connect_call` for E2B process execution
   - E2E test: bash tool via E2B sandbox

2. **Phase 1 (Follow-up)**:
   - Bidirectional streaming (stdin to running process)
   - Process lifecycle management (kill, signal)

## Consequences

### Positive
- WASM modules can execute processes in E2B sandboxes via Connect protocol
- No protobuf dependency in WASM modules — clean JSON interface
- Reuses existing Cedar authorization — no policy changes needed
- Minimal API surface: one new trait method, one new FFI function

### Negative
- Reads full response before returning (no true streaming to WASM) — acceptable for command output which is bounded
- Response buffer limit (1MB default) constrains output size

### Risks
- E2B envd may change Connect API between versions — mitigated by using stable `process.Process/Start` endpoint
- Large command outputs may exceed buffer — mitigated by existing resource limits (configurable per module)

### DST Compliance
- `ProductionWasmHost::connect_call` uses `reqwest` (same as `http_call`) — no new determinism concerns
- `SimWasmHost::connect_call` returns canned responses from `BTreeMap` — fully deterministic
- `host_connect_call` linker uses `tokio::task::block_in_place()` same as `host_http_call` — `// determinism-ok: async bridge`

## Non-Goals

- **gRPC support**: Full gRPC (HTTP/2, binary protobuf) is out of scope. Connect's HTTP/1.1 JSON mode is sufficient.
- **Bidirectional streaming**: This ADR covers unary and server-streaming only. Stdin piping is deferred.
- **Generic RPC framework**: This is specifically for Connect protocol, not a general-purpose RPC host function.

## Alternatives Considered

1. **Parse Connect frames in WASM** — Rejected. WASM modules would need to handle binary parsing, and `host_http_call` returns strings which can't carry binary frame prefixes faithfully.

2. **Add `host_grpc_call`** — Rejected for now. gRPC requires HTTP/2 multiplexing, binary protobuf encoding, and more complex framing. Connect's HTTP/1.1 JSON mode provides the same functionality with much less complexity. Can be added later if needed for non-Connect services.

3. **Proxy through a sidecar** — Rejected. Adding a REST-to-Connect proxy (Python/Go sidecar) adds deployment complexity and another failure point. The host function approach is self-contained.
