//! Host function linker: registers all `env.*` imports for WASM modules.
//!
//! Each host function bridges WASM linear memory to Rust capabilities
//! (logging, secrets, HTTP, streaming, caching, hashing). Functions are
//! linked once per invocation via a fresh `Linker<HostState>`.

use std::collections::BTreeMap;
use std::future::Future;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use wasmtime::{Caller, Linker};

use super::{HostState, WasmError};

/// Minimal deadline for host-side async calls entered after the invocation
/// budget has already been consumed.
///
/// The outer wrapper is defensive: if the inner host implementation timeout
/// fails to fire, this deadline guarantees the guest gets a result instead of
/// pinning the entity actor until passivation.
const MIN_HOST_CALL_OUTER_TIMEOUT: Duration = Duration::from_millis(1);

/// Run an async host-side future from a synchronous WASM FFI context with an
/// outer deadline derived from the current invocation budget.
///
/// Returns `Err(())` on outer timeout (logged via `tracing::warn`); the
/// caller should translate this to the WASM ABI's error sentinel (`-1`).
///
/// The `label` is used in the timeout log line so operators can tell which
/// host function hit the deadline without attaching a debugger.
pub(crate) fn run_host_call_with_timeout<T, F>(
    label: &'static str,
    deadline: Duration,
    fut: F,
) -> Result<T, ()>
where
    F: Future<Output = T>,
{
    run_host_call_with_timeout_impl(label, deadline.max(MIN_HOST_CALL_OUTER_TIMEOUT), fut)
}

/// Implementation seam for `run_host_call_with_timeout` that accepts the
/// deadline as an argument so tests can drive the timeout path with a short
/// real duration (paused-time testing is incompatible with this code path
/// because `block_in_place` requires the multi-threaded runtime).
fn run_host_call_with_timeout_impl<T, F>(
    label: &'static str,
    deadline: Duration,
    fut: F,
) -> Result<T, ()>
where
    F: Future<Output = T>,
{
    let outcome = tokio::task::block_in_place(|| {
        // determinism-ok: blocking bridge for WASM host call
        tokio::runtime::Handle::current()
            .block_on(async move { tokio::time::timeout(deadline, fut).await })
    });

    match outcome {
        Ok(value) => Ok(value),
        Err(_elapsed) => {
            tracing::warn!(
                host_fn = label,
                timeout_secs = deadline.as_secs(),
                timeout_ms = deadline.as_millis() as u64,
                "WASM host call exceeded outer deadline; returning error to guest"
            );
            Err(())
        }
    }
}

fn redact_url_for_log(url: &str) -> String {
    let without_fragment = url.split('#').next().unwrap_or(url);
    without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment)
        .to_string()
}

/// Outcome of resolving an entity-state field against a `HostState`.
///
/// Pulled out of the `host_read_field` closure so the pure logic is unit-testable
/// without a wasmtime instance.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FieldResolution {
    /// Bytes to hand back to the WASM guest.
    Bytes(Vec<u8>),
    /// Field not present in `entity_state.fields`.
    NotFound,
    /// Field is a blob ref but the pre-fetch `blob_cache` is missing the key.
    BlobRefMissing { key: String },
    /// JSON parse or unexpected shape; caller should treat as host error.
    HostError,
}

/// True if a `[ptr, ptr + len)` read lies fully within a guest linear memory of
/// `mem_size` bytes. Checked BEFORE allocating a read buffer so a guest-supplied
/// `len` can't force a large host allocation ahead of wasmtime's own bounds check
/// (ARN-226). `checked_add` also rejects a `ptr + len` that overflows `usize`.
pub(super) fn guest_read_bounds_ok(mem_size: usize, ptr: usize, len: usize) -> bool {
    ptr.checked_add(len).is_some_and(|end| end <= mem_size)
}

fn read_guest_string(caller: &mut Caller<'_, HostState>, ptr: i32, len: i32) -> Result<String, ()> {
    if ptr < 0 || len < 0 {
        return Err(());
    }
    let memory = caller.get_export("memory").and_then(|e| e.into_memory());
    let Some(memory) = memory else {
        return Err(());
    };
    if !guest_read_bounds_ok(memory.data_size(&mut *caller), ptr as usize, len as usize) {
        tracing::warn!(
            host_fn = "read_guest_string",
            ptr,
            len,
            "guest string read range exceeds linear memory; returning error before allocating"
        );
        return Err(());
    }
    let mut buf = vec![0u8; len as usize];
    memory
        .read(&mut *caller, ptr as usize, &mut buf)
        .map_err(|_| ())?;
    String::from_utf8(buf).map_err(|_| ())
}

/// Read `len` bytes of guest memory at `ptr`.
///
/// On failure (negative pointer/length or out-of-bounds read) emits a
/// `tracing::warn!` naming the host function and operand being read, and
/// returns `Err(())` so the caller can return its ABI error sentinel
/// instead of acting on uninitialized buffer contents.
pub(super) fn read_guest_bytes(
    caller: &Caller<'_, HostState>,
    memory: &wasmtime::Memory,
    ptr: i32,
    len: i32,
    host_fn: &'static str,
    what: &'static str,
) -> Result<Vec<u8>, ()> {
    if ptr < 0 || len < 0 {
        tracing::warn!(
            host_fn,
            operand = what,
            ptr,
            len,
            "guest passed negative pointer or length; returning error to guest"
        );
        return Err(());
    }
    if !guest_read_bounds_ok(memory.data_size(caller), ptr as usize, len as usize) {
        tracing::warn!(
            host_fn,
            operand = what,
            ptr,
            len,
            "guest read range exceeds linear memory; returning error before allocating"
        );
        return Err(());
    }
    let mut buf = vec![0u8; len as usize];
    if let Err(error) = memory.read(caller, ptr as usize, &mut buf) {
        tracing::warn!(
            host_fn,
            operand = what,
            error = %error,
            "guest memory read failed; returning error to guest"
        );
        return Err(());
    }
    Ok(buf)
}

/// Read `len` bytes of guest memory at `ptr` as a lossy UTF-8 string.
/// Same warn-and-`Err(())` contract as [`read_guest_bytes`].
fn read_guest_lossy(
    caller: &Caller<'_, HostState>,
    memory: &wasmtime::Memory,
    ptr: i32,
    len: i32,
    host_fn: &'static str,
    what: &'static str,
) -> Result<String, ()> {
    let buf = read_guest_bytes(caller, memory, ptr, len, host_fn, what)?;
    // Take the buffer by value when it is already valid UTF-8 (the common case);
    // only the invalid path pays for a second, lossy copy.
    Ok(match String::from_utf8(buf) {
        Ok(text) => text,
        Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
    })
}

/// Write `bytes` into guest memory at `ptr`.
///
/// On failure emits a `tracing::warn!` naming the host function and operand
/// being written, and returns `Err(())` so the caller can return its ABI
/// error sentinel instead of reporting success for a write that never landed.
fn write_guest_bytes(
    caller: &mut Caller<'_, HostState>,
    memory: &wasmtime::Memory,
    ptr: i32,
    bytes: &[u8],
    host_fn: &'static str,
    what: &'static str,
) -> Result<(), ()> {
    if ptr < 0 {
        tracing::warn!(
            host_fn,
            operand = what,
            ptr,
            "guest passed negative pointer; returning error to guest"
        );
        return Err(());
    }
    if let Err(error) = memory.write(caller, ptr as usize, bytes) {
        tracing::warn!(
            host_fn,
            operand = what,
            error = %error,
            "guest memory write failed; returning error to guest"
        );
        return Err(());
    }
    Ok(())
}

/// Parse a guest-supplied headers buffer (JSON array of `[key, value]`
/// pairs). On parse failure emits a `tracing::warn!` and returns `Err(())`
/// so the caller fails the call instead of silently sending empty headers.
fn parse_guest_headers(hdr_buf: &[u8], host_fn: &'static str) -> Result<Vec<(String, String)>, ()> {
    match serde_json::from_slice(hdr_buf) {
        Ok(headers) => Ok(headers),
        Err(error) => {
            tracing::warn!(
                host_fn,
                error = %error,
                "guest passed malformed headers JSON; failing the call"
            );
            Err(())
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct HostHttpBatchRequest {
    method: String,
    url: String,
    #[serde(default)]
    headers: Vec<(String, String)>,
    #[serde(default)]
    body: String,
}

#[derive(Debug, serde::Serialize)]
struct HostHttpBatchResponse {
    status: u16,
    body: String,
}

/// Resolve an entity-state field against the invocation context JSON and the
/// per-invocation blob cache. Plain strings come back as UTF-8 bytes (unquoted);
/// blob-ref envelopes come back as the decoded blob payload. See ADR-0046.
pub(crate) fn resolve_field_bytes(
    context_json: &str,
    blob_cache: &BTreeMap<String, Vec<u8>>,
    field_name: &str,
) -> FieldResolution {
    let Ok(ctx_value) = serde_json::from_str::<serde_json::Value>(context_json) else {
        return FieldResolution::HostError;
    };
    let field_value = ctx_value
        .get("entity_state")
        .and_then(|es| es.get("fields"))
        .and_then(|f| f.get(field_name))
        .cloned();
    let Some(field_value) = field_value else {
        return FieldResolution::NotFound;
    };

    if let Some(blob_key) = field_value
        .get("__temper_blob_ref")
        .and_then(|k| k.as_str())
    {
        return match blob_cache.get(blob_key) {
            Some(bytes) => FieldResolution::Bytes(bytes.clone()),
            None => FieldResolution::BlobRefMissing {
                key: blob_key.to_string(),
            },
        };
    }

    let bytes = match &field_value {
        serde_json::Value::String(s) => s.as_bytes().to_vec(),
        serde_json::Value::Null => Vec::new(),
        _ => match serde_json::to_vec(&field_value) {
            Ok(b) => b,
            Err(_) => return FieldResolution::HostError,
        },
    };
    FieldResolution::Bytes(bytes)
}

fn record_context_json_on_span(span: &tracing::Span, context_json: &str, workflow_step: &str) {
    span.record("workflow_step", workflow_step);
    let Ok(ctx) = serde_json::from_str::<serde_json::Value>(context_json) else {
        return;
    };
    if let Some(value) = ctx.get("tenant").and_then(serde_json::Value::as_str) {
        span.record("tenant", value);
    }
    if let Some(value) = ctx.get("entity_type").and_then(serde_json::Value::as_str) {
        span.record("entity_type", value);
    }
    if let Some(value) = ctx.get("entity_id").and_then(serde_json::Value::as_str) {
        span.record("entity_id", value);
    }
    if let Some(value) = ctx
        .get("trigger_action")
        .and_then(serde_json::Value::as_str)
    {
        span.record("trigger_action", value);
        span.record("action_name", value);
    }
    if let Some(value) = ctx.get("wasm_module").and_then(serde_json::Value::as_str) {
        span.record("wasm_module", value);
    }
    if let Some(value) = ctx.get("agent_id").and_then(serde_json::Value::as_str) {
        span.record("agent_id", value);
    }
    if let Some(value) = ctx
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            let entity_type = ctx.get("entity_type").and_then(serde_json::Value::as_str)?;
            (entity_type == "Session")
                .then(|| ctx.get("entity_id").and_then(serde_json::Value::as_str))
                .flatten()
        })
    {
        span.record("session_id", value);
    }
    if let Some(value) = ctx
        .get("workflow_root_entity_type")
        .and_then(serde_json::Value::as_str)
    {
        span.record("workflow_root_entity_type", value);
    }
    if let Some(value) = ctx
        .get("workflow_root_entity_id")
        .and_then(serde_json::Value::as_str)
    {
        span.record("workflow_root_entity_id", value);
    }
    if let Some(value) = ctx
        .get("workflow_run_id")
        .and_then(serde_json::Value::as_str)
    {
        span.record("workflow_run_id", value);
    }
}

/// Link all host functions into the WASM linker.
pub(super) fn link_host_functions(linker: &mut Linker<HostState>) -> Result<(), WasmError> {
    // host_log(level_ptr, level_len, msg_ptr, msg_len)
    linker
        .func_wrap(
            "env",
            "host_log",
            |mut caller: Caller<'_, HostState>,
             level_ptr: i32,
             level_len: i32,
             msg_ptr: i32,
             msg_len: i32| {
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let Some(memory) = memory else {
                    return;
                };
                let Ok(level) =
                    read_guest_lossy(&caller, &memory, level_ptr, level_len, "host_log", "level")
                else {
                    return;
                };
                let Ok(msg) =
                    read_guest_lossy(&caller, &memory, msg_ptr, msg_len, "host_log", "message")
                else {
                    return;
                };
                let _guest_span = caller.data().guest_spans.enter_active();
                caller.data().host.log(&level, &msg);
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_log: {e}")))?;

    // host_get_context(buf_ptr, buf_len) -> actual_len, or -1 on memory error
    linker
        .func_wrap(
            "env",
            "host_get_context",
            |mut caller: Caller<'_, HostState>, buf_ptr: i32, buf_len: i32| -> i32 {
                let ctx_json = caller.data().context_json.clone();
                let ctx_bytes = ctx_json.as_bytes();
                if (buf_len as usize) < ctx_bytes.len() {
                    return ctx_bytes.len() as i32; // Return needed size
                }
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let Some(memory) = memory else {
                    return -1;
                };
                if write_guest_bytes(
                    &mut caller,
                    &memory,
                    buf_ptr,
                    ctx_bytes,
                    "host_get_context",
                    "context",
                )
                .is_err()
                {
                    return -1;
                }
                ctx_bytes.len() as i32
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_get_context: {e}")))?;

    // host_set_result(ptr, len)
    linker
        .func_wrap(
            "env",
            "host_set_result",
            |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| {
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let Some(memory) = memory else {
                    return;
                };
                let Ok(buf) =
                    read_guest_bytes(&caller, &memory, ptr, len, "host_set_result", "result")
                else {
                    return;
                };
                if let Ok(s) = String::from_utf8(buf) {
                    caller.data_mut().result_json = Some(s);
                }
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_set_result: {e}")))?;

    // host_emit_progress(ptr, len) -> i32
    linker
        .func_wrap(
            "env",
            "host_emit_progress",
            |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i32 {
                // ARN-226: read via the bounds-checked helper so a guest-supplied
                // `len` can't drive a host allocation ahead of the bounds check.
                let Ok(payload) = read_guest_string(&mut caller, ptr, len) else {
                    return -1;
                };
                let _guest_span = caller.data().guest_spans.enter_active();
                match caller.data().host.emit_progress(&payload) {
                    Ok(()) => 0,
                    Err(_) => -1,
                }
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_emit_progress: {e}")))?;

    // host_emit_wide_event(ptr, len) -> i32
    linker
        .func_wrap(
            "env",
            "host_emit_wide_event",
            |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i32 {
                // ARN-226: read via the bounds-checked helper so a guest-supplied
                // `len` can't drive a host allocation ahead of the bounds check.
                let Ok(payload) = read_guest_string(&mut caller, ptr, len) else {
                    return -1;
                };
                let _guest_span = caller.data().guest_spans.enter_active();
                match caller.data().host.emit_wide_event(&payload) {
                    Ok(()) => 0,
                    Err(_) => -1,
                }
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_emit_wide_event: {e}")))?;

    // host_log_structured(ptr, len) -> i32
    linker
        .func_wrap(
            "env",
            "host_log_structured",
            |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i32 {
                // ARN-226: read via the bounds-checked helper so a guest-supplied
                // `len` can't drive a host allocation ahead of the bounds check.
                let Ok(payload) = read_guest_string(&mut caller, ptr, len) else {
                    return -1;
                };
                let _guest_span = caller.data().guest_spans.enter_active();
                match caller.data().host.log_structured(&payload) {
                    Ok(()) => 0,
                    Err(_) => -1,
                }
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_log_structured: {e}")))?;

    // host_emit_metric(ptr, len) -> i32
    linker
        .func_wrap(
            "env",
            "host_emit_metric",
            |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i32 {
                // ARN-226: read via the bounds-checked helper so a guest-supplied
                // `len` can't drive a host allocation ahead of the bounds check.
                let Ok(payload) = read_guest_string(&mut caller, ptr, len) else {
                    return -1;
                };
                let _guest_span = caller.data().guest_spans.enter_active();
                match caller.data().host.emit_metric(&payload) {
                    Ok(()) => 0,
                    Err(_) => -1,
                }
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_emit_metric: {e}")))?;

    // host_start_span(ptr, len) -> i64
    linker
        .func_wrap(
            "env",
            "host_start_span",
            |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i64 {
                let Ok(payload) = read_guest_string(&mut caller, ptr, len) else {
                    return -1;
                };
                match caller.data_mut().guest_spans.start_span(&payload) {
                    Ok(span_id) => span_id,
                    Err(error) => {
                        tracing::warn!(error = %error, "host_start_span failed");
                        -1
                    }
                }
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_start_span: {e}")))?;

    // host_add_span_event(span_id, ptr, len) -> i32
    linker
        .func_wrap(
            "env",
            "host_add_span_event",
            |mut caller: Caller<'_, HostState>, span_id: i64, ptr: i32, len: i32| -> i32 {
                let Ok(payload) = read_guest_string(&mut caller, ptr, len) else {
                    return -1;
                };
                match caller
                    .data_mut()
                    .guest_spans
                    .add_span_event(span_id, &payload)
                {
                    Ok(()) => 0,
                    Err(error) => {
                        tracing::warn!(span_id, error = %error, "host_add_span_event failed");
                        -1
                    }
                }
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_add_span_event: {e}")))?;

    // host_set_span_attributes(span_id, ptr, len) -> i32
    linker
        .func_wrap(
            "env",
            "host_set_span_attributes",
            |mut caller: Caller<'_, HostState>, span_id: i64, ptr: i32, len: i32| -> i32 {
                let Ok(payload) = read_guest_string(&mut caller, ptr, len) else {
                    return -1;
                };
                match caller
                    .data_mut()
                    .guest_spans
                    .set_span_attributes(span_id, &payload)
                {
                    Ok(()) => 0,
                    Err(error) => {
                        tracing::warn!(
                            span_id,
                            error = %error,
                            "host_set_span_attributes failed"
                        );
                        -1
                    }
                }
            },
        )
        .map_err(|e| {
            WasmError::Compilation(format!("failed to link host_set_span_attributes: {e}"))
        })?;

    // host_end_span(span_id, ptr, len) -> i32
    linker
        .func_wrap(
            "env",
            "host_end_span",
            |mut caller: Caller<'_, HostState>, span_id: i64, ptr: i32, len: i32| -> i32 {
                let Ok(payload) = read_guest_string(&mut caller, ptr, len) else {
                    return -1;
                };
                match caller.data_mut().guest_spans.end_span(span_id, &payload) {
                    Ok(()) => 0,
                    Err(error) => {
                        tracing::warn!(span_id, error = %error, "host_end_span failed");
                        -1
                    }
                }
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_end_span: {e}")))?;

    // host_get_secret(key_ptr, key_len, buf_ptr, buf_len) -> actual_len (-1 on error)
    linker
        .func_wrap(
            "env",
            "host_get_secret",
            |mut caller: Caller<'_, HostState>,
             key_ptr: i32,
             key_len: i32,
             buf_ptr: i32,
             buf_len: i32|
             -> i32 {
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let Some(memory) = memory else { return -1 };

                let Ok(key) =
                    read_guest_lossy(&caller, &memory, key_ptr, key_len, "host_get_secret", "key")
                else {
                    return -1;
                };

                let _guest_span = caller.data().guest_spans.enter_active();
                match caller.data().host.get_secret(&key) {
                    Ok(secret) => {
                        let secret_bytes = secret.as_bytes();
                        if (buf_len as usize) < secret_bytes.len() {
                            return secret_bytes.len() as i32;
                        }
                        if write_guest_bytes(
                            &mut caller,
                            &memory,
                            buf_ptr,
                            secret_bytes,
                            "host_get_secret",
                            "secret",
                        )
                        .is_err()
                        {
                            return -1;
                        }
                        secret_bytes.len() as i32
                    }
                    Err(_) => -1,
                }
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_get_secret: {e}")))?;

    // host_http_call(method_ptr, method_len, url_ptr, url_len,
    //                headers_ptr, headers_len, body_ptr, body_len,
    //                result_buf_ptr, result_buf_len) -> i32
    // Returns: bytes written to result_buf (status_code\nbody), or -1 on error, -2 if buf too small
    linker
        .func_wrap(
            "env",
            "host_http_call",
            |mut caller: Caller<'_, HostState>,
             method_ptr: i32,
             method_len: i32,
             url_ptr: i32,
             url_len: i32,
             headers_ptr: i32,
             headers_len: i32,
             body_ptr: i32,
             body_len: i32,
             result_buf_ptr: i32,
             result_buf_len: i32|
             -> i32 {
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let Some(memory) = memory else {
                    return -1;
                };

                // Read method
                let Ok(method) = read_guest_lossy(
                    &caller,
                    &memory,
                    method_ptr,
                    method_len,
                    "host_http_call",
                    "method",
                ) else {
                    return -1;
                };

                // Read URL
                let Ok(url) =
                    read_guest_lossy(&caller, &memory, url_ptr, url_len, "host_http_call", "url")
                else {
                    return -1;
                };

                // Read headers (JSON array of [key, value] pairs)
                let headers: Vec<(String, String)> = if headers_len > 0 {
                    let Ok(hdr_buf) = read_guest_bytes(
                        &caller,
                        &memory,
                        headers_ptr,
                        headers_len,
                        "host_http_call",
                        "headers",
                    ) else {
                        return -1;
                    };
                    let Ok(headers) = parse_guest_headers(&hdr_buf, "host_http_call") else {
                        return -1;
                    };
                    headers
                } else {
                    vec![]
                };

                // Read body
                let body = if body_len > 0 {
                    let Ok(body) = read_guest_lossy(
                        &caller,
                        &memory,
                        body_ptr,
                        body_len,
                        "host_http_call",
                        "body",
                    ) else {
                        return -1;
                    };
                    body
                } else {
                    String::new()
                };

                // Bridge async -> sync with an outer deadline so a hanging
                // host call can never pin the actor indefinitely.
                let _guest_span = caller.data().guest_spans.enter_active();
                let host = caller.data().host.clone();
                let deadline = caller.data().remaining_host_call_timeout();
                let Ok(result) =
                    run_host_call_with_timeout("host_http_call", deadline, async move {
                        host.http_call(&method, &url, &headers, &body).await
                    })
                else {
                    return -1;
                };

                match result {
                    Ok((status, resp_body)) => {
                        let response = format!("{status}\n{resp_body}");
                        let resp_bytes = response.as_bytes();
                        if resp_bytes.len() > result_buf_len as usize {
                            return -2; // buffer too small
                        }
                        if write_guest_bytes(
                            &mut caller,
                            &memory,
                            result_buf_ptr,
                            resp_bytes,
                            "host_http_call",
                            "response",
                        )
                        .is_err()
                        {
                            return -1;
                        }
                        resp_bytes.len() as i32
                    }
                    Err(_) => -1,
                }
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_http_call: {e}")))?;

    // host_http_call_batch(requests_ptr, requests_len, result_buf_ptr, result_buf_len) -> i32
    // Requests are a JSON array of {method,url,headers,body}. Returns bytes written
    // to result_buf (JSON array of {status,body}), or -1 on error, -2 if buf too small.
    linker
        .func_wrap(
            "env",
            "host_http_call_batch",
            |mut caller: Caller<'_, HostState>,
             requests_ptr: i32,
             requests_len: i32,
             result_buf_ptr: i32,
             result_buf_len: i32|
             -> i32 {
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let Some(memory) = memory else {
                    return -1;
                };

                let requests: Vec<HostHttpBatchRequest> = if requests_len > 0 {
                    let Ok(requests_buf) = read_guest_bytes(
                        &caller,
                        &memory,
                        requests_ptr,
                        requests_len,
                        "host_http_call_batch",
                        "requests",
                    ) else {
                        return -1;
                    };
                    match serde_json::from_slice(&requests_buf) {
                        Ok(parsed) => parsed,
                        Err(_) => return -1,
                    }
                } else {
                    Vec::new()
                };

                let _guest_span = caller.data().guest_spans.enter_active();
                let host = caller.data().host.clone();
                let deadline = caller.data().remaining_host_call_timeout();
                let Ok(result) =
                    run_host_call_with_timeout("host_http_call_batch", deadline, async move {
                        host.http_call_batch(
                            &requests
                                .iter()
                                .map(|request| crate::host_trait::HttpBatchRequest {
                                    method: request.method.clone(),
                                    url: request.url.clone(),
                                    headers: request.headers.clone(),
                                    body: request.body.clone(),
                                })
                                .collect::<Vec<_>>(),
                        )
                        .await
                    })
                else {
                    return -1;
                };

                match result {
                    Ok(responses) => {
                        let response_json = serde_json::to_string(
                            &responses
                                .into_iter()
                                .map(|response| HostHttpBatchResponse {
                                    status: response.status,
                                    body: response.body,
                                })
                                .collect::<Vec<_>>(),
                        )
                        .unwrap_or_else(|_| "[]".into());
                        let response_bytes = response_json.as_bytes();
                        if response_bytes.len() > result_buf_len as usize {
                            return -2;
                        }
                        if write_guest_bytes(
                            &mut caller,
                            &memory,
                            result_buf_ptr,
                            response_bytes,
                            "host_http_call_batch",
                            "responses",
                        )
                        .is_err()
                        {
                            return -1;
                        }
                        response_bytes.len() as i32
                    }
                    Err(_) => -1,
                }
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_http_call_batch: {e}")))?;

    // host_connect_call(url_ptr, url_len, headers_ptr, headers_len,
    //                   body_ptr, body_len, result_buf_ptr, result_buf_len) -> i32
    // Makes a Connect protocol server-streaming RPC call.
    // Returns: bytes written to result_buf (JSON array of frame payloads),
    // or -1 on error, -2 if buf too small.
    linker
        .func_wrap(
            "env",
            "host_connect_call",
            |mut caller: Caller<'_, HostState>,
             url_ptr: i32,
             url_len: i32,
             headers_ptr: i32,
             headers_len: i32,
             body_ptr: i32,
             body_len: i32,
             result_buf_ptr: i32,
             result_buf_len: i32|
             -> i32 {
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let Some(memory) = memory else {
                    return -1;
                };

                // Read URL
                let Ok(url) = read_guest_lossy(
                    &caller,
                    &memory,
                    url_ptr,
                    url_len,
                    "host_connect_call",
                    "url",
                ) else {
                    return -1;
                };

                // Read headers (JSON array of [key, value] pairs)
                let headers: Vec<(String, String)> = if headers_len > 0 {
                    let Ok(hdr_buf) = read_guest_bytes(
                        &caller,
                        &memory,
                        headers_ptr,
                        headers_len,
                        "host_connect_call",
                        "headers",
                    ) else {
                        return -1;
                    };
                    let Ok(headers) = parse_guest_headers(&hdr_buf, "host_connect_call") else {
                        return -1;
                    };
                    headers
                } else {
                    vec![]
                };

                // Read body
                let body = if body_len > 0 {
                    let Ok(body) = read_guest_lossy(
                        &caller,
                        &memory,
                        body_ptr,
                        body_len,
                        "host_connect_call",
                        "body",
                    ) else {
                        return -1;
                    };
                    body
                } else {
                    String::new()
                };

                // Bridge async -> sync with an outer deadline so a hanging
                // host call can never pin the actor indefinitely.
                let _guest_span = caller.data().guest_spans.enter_active();
                let host = caller.data().host.clone();
                let deadline = caller.data().remaining_host_call_timeout();
                let Ok(result) =
                    run_host_call_with_timeout("host_connect_call", deadline, async move {
                        host.connect_call(&url, &headers, &body).await
                    })
                else {
                    return -1;
                };

                match result {
                    Ok(frames) => {
                        let json = serde_json::to_string(&frames).unwrap_or_else(|_| "[]".into());
                        let json_bytes = json.as_bytes();
                        if json_bytes.len() > result_buf_len as usize {
                            return -2; // buffer too small
                        }
                        if write_guest_bytes(
                            &mut caller,
                            &memory,
                            result_buf_ptr,
                            json_bytes,
                            "host_connect_call",
                            "frames",
                        )
                        .is_err()
                        {
                            return -1;
                        }
                        json_bytes.len() as i32
                    }
                    Err(_) => -1,
                }
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_connect_call: {e}")))?;

    // host_http_call_stream(method_ptr, method_len, url_ptr, url_len,
    //                       headers_ptr, headers_len,
    //                       body_stream_id_ptr, body_stream_id_len,
    //                       response_stream_id_ptr, response_stream_id_len) -> i32
    // Returns HTTP status code, or -1 on error.
    // Bytes flow through StreamRegistry, never through WASM memory.
    #[allow(clippy::too_many_arguments)]
    linker
        .func_wrap(
            "env",
            "host_http_call_stream",
            |mut caller: Caller<'_, HostState>,
             method_ptr: i32,
             method_len: i32,
             url_ptr: i32,
             url_len: i32,
             headers_ptr: i32,
             headers_len: i32,
             body_stream_id_ptr: i32,
             body_stream_id_len: i32,
             response_stream_id_ptr: i32,
             response_stream_id_len: i32|
             -> i32 {
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let Some(memory) = memory else {
                    return -1;
                };

                // Read method
                let Ok(method) = read_guest_lossy(
                    &caller,
                    &memory,
                    method_ptr,
                    method_len,
                    "host_http_call_stream",
                    "method",
                ) else {
                    return -1;
                };

                // Read URL
                let Ok(url) = read_guest_lossy(
                    &caller,
                    &memory,
                    url_ptr,
                    url_len,
                    "host_http_call_stream",
                    "url",
                ) else {
                    return -1;
                };

                // Read headers (JSON array of [key, value] pairs)
                let headers: Vec<(String, String)> = if headers_len > 0 {
                    let Ok(hdr_buf) = read_guest_bytes(
                        &caller,
                        &memory,
                        headers_ptr,
                        headers_len,
                        "host_http_call_stream",
                        "headers",
                    ) else {
                        return -1;
                    };
                    let Ok(headers) = parse_guest_headers(&hdr_buf, "host_http_call_stream") else {
                        return -1;
                    };
                    headers
                } else {
                    vec![]
                };

                // Read body stream ID
                let body_stream_id = if body_stream_id_len > 0 {
                    let Ok(id) = read_guest_lossy(
                        &caller,
                        &memory,
                        body_stream_id_ptr,
                        body_stream_id_len,
                        "host_http_call_stream",
                        "body_stream_id",
                    ) else {
                        return -1;
                    };
                    id
                } else {
                    String::new()
                };

                // Read response stream ID
                let response_stream_id = if response_stream_id_len > 0 {
                    let Ok(id) = read_guest_lossy(
                        &caller,
                        &memory,
                        response_stream_id_ptr,
                        response_stream_id_len,
                        "host_http_call_stream",
                        "response_stream_id",
                    ) else {
                        return -1;
                    };
                    id
                } else {
                    String::new()
                };

                // Get request body from StreamRegistry (if stream ID provided)
                let body_bytes = if !body_stream_id.is_empty() {
                    let streams = caller.data().streams.read().expect("streams lock poisoned"); // ci-ok: infallible lock
                    streams
                        .get_stream(&body_stream_id)
                        .map(|b| b.to_vec())
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };

                // Bridge async -> sync with an outer deadline so a hanging
                // host call can never pin the actor indefinitely.
                let _guest_span = caller.data().guest_spans.enter_active();
                let host = caller.data().host.clone();
                let deadline = caller.data().remaining_host_call_timeout();
                let Ok(result) =
                    run_host_call_with_timeout("host_http_call_stream", deadline, async move {
                        host.http_call_binary(&method, &url, &headers, &body_bytes)
                            .await
                    })
                else {
                    return -1;
                };

                match result {
                    Ok((status, resp_bytes)) => {
                        // Store response bytes in StreamRegistry (if stream ID provided)
                        if !response_stream_id.is_empty() && !resp_bytes.is_empty() {
                            let mut streams = caller
                                .data()
                                .streams
                                .write()
                                .expect("streams lock poisoned"); // ci-ok: infallible lock
                            streams.store_stream(&response_stream_id, resp_bytes);
                        }
                        status as i32
                    }
                    Err(_) => -1,
                }
            },
        )
        .map_err(|e| {
            WasmError::Compilation(format!("failed to link host_http_call_stream: {e}"))
        })?;

    // host_cache_contains(key_ptr, key_len) -> i32
    // Returns 1 if cached, 0 if not.
    linker
        .func_wrap(
            "env",
            "host_cache_contains",
            |mut caller: Caller<'_, HostState>, key_ptr: i32, key_len: i32| -> i32 {
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let Some(memory) = memory else {
                    return 0;
                };

                let Ok(key) = read_guest_lossy(
                    &caller,
                    &memory,
                    key_ptr,
                    key_len,
                    "host_cache_contains",
                    "key",
                ) else {
                    // The ABI is boolean (1 = cached, 0 = not) with no error
                    // sentinel; treat an unreadable key as a miss so the
                    // guest falls back to fetching.
                    return 0;
                };
                let started = Instant::now();
                let _guest_span = caller.data().guest_spans.enter_active();
                let span = tracing::info_span!(
                    "wasm.host.cache_contains",
                    otel.name = "wasm.host.cache_contains",
                    cache.key = %key,
                    cache.hit = tracing::field::Empty,
                    duration_ms = tracing::field::Empty,
                    tenant = tracing::field::Empty,
                    wasm_module = tracing::field::Empty,
                    session_id = tracing::field::Empty,
                    agent_id = tracing::field::Empty,
                    entity_type = tracing::field::Empty,
                    entity_id = tracing::field::Empty,
                    trigger_action = tracing::field::Empty,
                    action_name = tracing::field::Empty,
                    workflow_step = tracing::field::Empty,
                    workflow_root_entity_type = tracing::field::Empty,
                    workflow_root_entity_id = tracing::field::Empty,
                    workflow_run_id = tracing::field::Empty,
                );
                record_context_json_on_span(&span, &caller.data().context_json, "cache_contains");
                let _guard = span.enter();

                let streams = caller.data().streams.read().expect("streams lock poisoned"); // ci-ok: infallible lock
                let hit = streams.cache_contains(&key);
                tracing::Span::current().record("cache.hit", hit);
                tracing::Span::current()
                    .record("duration_ms", started.elapsed().as_millis() as u64);
                if hit { 1 } else { 0 }
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_cache_contains: {e}")))?;

    // host_cache_to_stream(key_ptr, key_len, stream_id_ptr, stream_id_len) -> i32
    // Copies cached bytes to a stream. Returns byte count on success, -1 if not cached.
    linker
        .func_wrap(
            "env",
            "host_cache_to_stream",
            |mut caller: Caller<'_, HostState>,
             key_ptr: i32,
             key_len: i32,
             stream_id_ptr: i32,
             stream_id_len: i32|
             -> i32 {
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let Some(memory) = memory else {
                    return -1;
                };

                let Ok(key) = read_guest_lossy(
                    &caller,
                    &memory,
                    key_ptr,
                    key_len,
                    "host_cache_to_stream",
                    "key",
                ) else {
                    return -1;
                };

                let Ok(stream_id) = read_guest_lossy(
                    &caller,
                    &memory,
                    stream_id_ptr,
                    stream_id_len,
                    "host_cache_to_stream",
                    "stream_id",
                ) else {
                    return -1;
                };
                let started = Instant::now();
                let _guest_span = caller.data().guest_spans.enter_active();
                let span = tracing::info_span!(
                    "wasm.host.cache_to_stream",
                    otel.name = "wasm.host.cache_to_stream",
                    cache.key = %key,
                    stream.id = %stream_id,
                    cache.hit = tracing::field::Empty,
                    bytes = tracing::field::Empty,
                    duration_ms = tracing::field::Empty,
                    tenant = tracing::field::Empty,
                    wasm_module = tracing::field::Empty,
                    session_id = tracing::field::Empty,
                    agent_id = tracing::field::Empty,
                    entity_type = tracing::field::Empty,
                    entity_id = tracing::field::Empty,
                    trigger_action = tracing::field::Empty,
                    action_name = tracing::field::Empty,
                    workflow_step = tracing::field::Empty,
                    workflow_root_entity_type = tracing::field::Empty,
                    workflow_root_entity_id = tracing::field::Empty,
                    workflow_run_id = tracing::field::Empty,
                );
                record_context_json_on_span(&span, &caller.data().context_json, "cache_to_stream");
                let _guard = span.enter();

                let mut streams = caller
                    .data()
                    .streams
                    .write()
                    .expect("streams lock poisoned"); // ci-ok: infallible lock
                match streams.cache_to_stream(&key, &stream_id) {
                    Some(byte_count) => {
                        tracing::Span::current().record("cache.hit", true);
                        tracing::Span::current().record("bytes", byte_count as u64);
                        tracing::Span::current()
                            .record("duration_ms", started.elapsed().as_millis() as u64);
                        byte_count as i32
                    }
                    None => {
                        tracing::Span::current().record("cache.hit", false);
                        tracing::Span::current()
                            .record("duration_ms", started.elapsed().as_millis() as u64);
                        -1
                    }
                }
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_cache_to_stream: {e}")))?;

    // host_read_field(field_name_ptr, field_name_len, buf_ptr, buf_len) -> i32
    //
    // Resolves an entity-state field into a WASM memory buffer.
    // - Plain string values are written as their raw UTF-8 bytes (unquoted)
    //   so guests see identical bytes regardless of inline vs blob-ref storage.
    // - Non-string JSON values are written as their UTF-8 JSON serialization.
    // - Blob-ref values ({"__temper_blob_ref": "..."}) are resolved from the
    //   per-invocation blob_cache pre-populated by the dispatcher.
    //
    // Return contract (matches `host_get_context`):
    //   >= 0 with value <= buf_len — bytes written; read `value` bytes from buf_ptr.
    //   >  buf_len                 — needed buffer size; caller should resize + retry.
    //     -1                       — field not in entity_state.fields.
    //     -2                       — field is a blob ref; pre-fetch did not populate blob_cache.
    //     -3                       — generic host error (memory access, JSON parse).
    //
    // See ADR-0046.
    linker
        .func_wrap(
            "env",
            "host_read_field",
            |mut caller: Caller<'_, HostState>,
             field_name_ptr: i32,
             field_name_len: i32,
             buf_ptr: i32,
             buf_len: i32|
             -> i32 {
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let Some(memory) = memory else {
                    return -3;
                };

                // ARN-226: bounds-checked read before allocating.
                let Ok(field_name) = read_guest_lossy(
                    &caller,
                    &memory,
                    field_name_ptr,
                    field_name_len,
                    "host_read_field",
                    "field_name",
                ) else {
                    return -3;
                };
                let started = Instant::now();
                let _guest_span = caller.data().guest_spans.enter_active();
                let span = tracing::info_span!(
                    "wasm.host.read_field",
                    otel.name = "wasm.host.read_field",
                    field.name = %field_name,
                    result = tracing::field::Empty,
                    bytes = tracing::field::Empty,
                    duration_ms = tracing::field::Empty,
                    tenant = tracing::field::Empty,
                    wasm_module = tracing::field::Empty,
                    session_id = tracing::field::Empty,
                    agent_id = tracing::field::Empty,
                    entity_type = tracing::field::Empty,
                    entity_id = tracing::field::Empty,
                    trigger_action = tracing::field::Empty,
                    action_name = tracing::field::Empty,
                    workflow_step = tracing::field::Empty,
                    workflow_root_entity_type = tracing::field::Empty,
                    workflow_root_entity_id = tracing::field::Empty,
                    workflow_run_id = tracing::field::Empty,
                );
                record_context_json_on_span(&span, &caller.data().context_json, "read_field");
                let _guard = span.enter();

                let bytes = match resolve_field_bytes(
                    &caller.data().context_json,
                    &caller.data().blob_cache,
                    &field_name,
                ) {
                    FieldResolution::Bytes(b) => b,
                    FieldResolution::NotFound => {
                        tracing::Span::current().record("result", "not_found");
                        tracing::Span::current()
                            .record("duration_ms", started.elapsed().as_millis() as u64);
                        return -1;
                    }
                    FieldResolution::BlobRefMissing { key } => {
                        tracing::warn!(
                            field = %field_name,
                            blob_key = %key,
                            "host_read_field: blob ref not in prefetch cache"
                        );
                        tracing::Span::current().record("result", "blob_ref_missing");
                        tracing::Span::current()
                            .record("duration_ms", started.elapsed().as_millis() as u64);
                        return -2;
                    }
                    FieldResolution::HostError => {
                        tracing::Span::current().record("result", "host_error");
                        tracing::Span::current()
                            .record("duration_ms", started.elapsed().as_millis() as u64);
                        return -3;
                    }
                };

                let needed = bytes.len() as i32;
                if needed > buf_len {
                    // Buffer too small — signal needed size; caller retries with larger buf.
                    tracing::Span::current().record("result", "buffer_too_small");
                    tracing::Span::current().record("bytes", needed as u64);
                    tracing::Span::current()
                        .record("duration_ms", started.elapsed().as_millis() as u64);
                    return needed;
                }
                if memory.write(&mut caller, buf_ptr as usize, &bytes).is_err() {
                    tracing::Span::current().record("result", "memory_write_error");
                    tracing::Span::current()
                        .record("duration_ms", started.elapsed().as_millis() as u64);
                    return -3;
                }
                tracing::Span::current().record("result", "ok");
                tracing::Span::current().record("bytes", needed as u64);
                tracing::Span::current()
                    .record("duration_ms", started.elapsed().as_millis() as u64);
                needed
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_read_field: {e}")))?;

    // host_cache_from_stream(key_ptr, key_len, stream_id_ptr, stream_id_len) -> i32
    // Caches bytes from a stream. Returns 0 on success, -1 on error.
    linker
        .func_wrap(
            "env",
            "host_cache_from_stream",
            |mut caller: Caller<'_, HostState>,
             key_ptr: i32,
             key_len: i32,
             stream_id_ptr: i32,
             stream_id_len: i32|
             -> i32 {
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let Some(memory) = memory else {
                    return -1;
                };

                let Ok(key) = read_guest_lossy(
                    &caller,
                    &memory,
                    key_ptr,
                    key_len,
                    "host_cache_from_stream",
                    "key",
                ) else {
                    return -1;
                };

                let Ok(stream_id) = read_guest_lossy(
                    &caller,
                    &memory,
                    stream_id_ptr,
                    stream_id_len,
                    "host_cache_from_stream",
                    "stream_id",
                ) else {
                    return -1;
                };
                let started = Instant::now();
                let _guest_span = caller.data().guest_spans.enter_active();
                let span = tracing::info_span!(
                    "wasm.host.cache_from_stream",
                    otel.name = "wasm.host.cache_from_stream",
                    cache.key = %key,
                    stream.id = %stream_id,
                    success = tracing::field::Empty,
                    bytes = tracing::field::Empty,
                    duration_ms = tracing::field::Empty,
                    tenant = tracing::field::Empty,
                    wasm_module = tracing::field::Empty,
                    session_id = tracing::field::Empty,
                    agent_id = tracing::field::Empty,
                    entity_type = tracing::field::Empty,
                    entity_id = tracing::field::Empty,
                    trigger_action = tracing::field::Empty,
                    action_name = tracing::field::Empty,
                    workflow_step = tracing::field::Empty,
                    workflow_root_entity_type = tracing::field::Empty,
                    workflow_root_entity_id = tracing::field::Empty,
                    workflow_run_id = tracing::field::Empty,
                );
                record_context_json_on_span(
                    &span,
                    &caller.data().context_json,
                    "cache_from_stream",
                );
                let _guard = span.enter();

                let mut streams = caller
                    .data()
                    .streams
                    .write()
                    .expect("streams lock poisoned"); // ci-ok: infallible lock
                // Read bytes from stream without consuming it
                let bytes = match streams.get_stream(&stream_id) {
                    Some(b) => b.to_vec(),
                    None => {
                        tracing::Span::current().record("success", false);
                        tracing::Span::current()
                            .record("duration_ms", started.elapsed().as_millis() as u64);
                        return -1;
                    }
                };
                let bytes_len = bytes.len() as u64;
                streams.cache_put(&key, bytes);
                tracing::Span::current().record("success", true);
                tracing::Span::current().record("bytes", bytes_len);
                tracing::Span::current()
                    .record("duration_ms", started.elapsed().as_millis() as u64);
                0
            },
        )
        .map_err(|e| {
            WasmError::Compilation(format!("failed to link host_cache_from_stream: {e}"))
        })?;

    // host_hash_stream(stream_id_ptr, stream_id_len,
    //                  algorithm_ptr, algorithm_len,
    //                  result_buf_ptr, result_buf_len) -> i32
    // Computes hash of stream bytes. Returns bytes written to result_buf, or -1 on error.
    // Algorithm chosen by WASM (hot-reloadable): "sha256", "blake3", etc.
    linker
        .func_wrap(
            "env",
            "host_hash_stream",
            |mut caller: Caller<'_, HostState>,
             stream_id_ptr: i32,
             stream_id_len: i32,
             algorithm_ptr: i32,
             algorithm_len: i32,
             result_buf_ptr: i32,
             result_buf_len: i32|
             -> i32 {
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let Some(memory) = memory else {
                    return -1;
                };

                // Read stream ID
                let Ok(stream_id) = read_guest_lossy(
                    &caller,
                    &memory,
                    stream_id_ptr,
                    stream_id_len,
                    "host_hash_stream",
                    "stream_id",
                ) else {
                    return -1;
                };

                // Read algorithm
                let Ok(algorithm) = read_guest_lossy(
                    &caller,
                    &memory,
                    algorithm_ptr,
                    algorithm_len,
                    "host_hash_stream",
                    "algorithm",
                ) else {
                    return -1;
                };
                let started = Instant::now();
                let _guest_span = caller.data().guest_spans.enter_active();
                let span = tracing::info_span!(
                    "wasm.host.hash_stream",
                    otel.name = "wasm.host.hash_stream",
                    stream.id = %stream_id,
                    hash.algorithm = %algorithm,
                    success = tracing::field::Empty,
                    bytes = tracing::field::Empty,
                    duration_ms = tracing::field::Empty,
                    tenant = tracing::field::Empty,
                    wasm_module = tracing::field::Empty,
                    session_id = tracing::field::Empty,
                    agent_id = tracing::field::Empty,
                    entity_type = tracing::field::Empty,
                    entity_id = tracing::field::Empty,
                    trigger_action = tracing::field::Empty,
                    action_name = tracing::field::Empty,
                    workflow_step = tracing::field::Empty,
                    workflow_root_entity_type = tracing::field::Empty,
                    workflow_root_entity_id = tracing::field::Empty,
                    workflow_run_id = tracing::field::Empty,
                );
                record_context_json_on_span(&span, &caller.data().context_json, "hash_stream");
                let _guard = span.enter();

                // Hash stream bytes in-place (no clone)
                let streams = caller.data().streams.read().expect("streams lock poisoned"); // ci-ok: infallible lock
                let Some(bytes) = streams.get_stream(&stream_id) else {
                    tracing::Span::current().record("success", false);
                    tracing::Span::current()
                        .record("duration_ms", started.elapsed().as_millis() as u64);
                    return -1;
                };
                tracing::Span::current().record("bytes", bytes.len() as u64);

                let hex_hash = match algorithm.as_str() {
                    "sha256" => {
                        let mut hasher = Sha256::new();
                        hasher.update(bytes);
                        format!("sha256:{:x}", hasher.finalize())
                    }
                    _ => {
                        tracing::Span::current().record("success", false);
                        tracing::Span::current()
                            .record("duration_ms", started.elapsed().as_millis() as u64);
                        return -1;
                    }
                };
                drop(streams);

                // Write hex hash to result buffer
                let hash_bytes = hex_hash.as_bytes();
                if hash_bytes.len() > result_buf_len as usize {
                    tracing::Span::current().record("success", false);
                    tracing::Span::current()
                        .record("duration_ms", started.elapsed().as_millis() as u64);
                    return -1; // buffer too small
                }
                if write_guest_bytes(
                    &mut caller,
                    &memory,
                    result_buf_ptr,
                    hash_bytes,
                    "host_hash_stream",
                    "hash",
                )
                .is_err()
                {
                    tracing::Span::current().record("success", false);
                    tracing::Span::current()
                        .record("duration_ms", started.elapsed().as_millis() as u64);
                    return -1;
                }
                tracing::Span::current().record("success", true);
                tracing::Span::current()
                    .record("duration_ms", started.elapsed().as_millis() as u64);
                hash_bytes.len() as i32
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_hash_stream: {e}")))?;

    // host_get_time(buf_ptr, buf_len) -> i32
    // Writes the current UTC time as "YYYYMMDDTHHMMSSz" (Sig V4 format) into buf.
    // Returns bytes written, or -1 on error.
    linker
        .func_wrap(
            "env",
            "host_get_time",
            |mut caller: Caller<'_, HostState>, buf_ptr: i32, buf_len: i32| -> i32 {
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let Some(memory) = memory else {
                    return -1;
                };

                let now = chrono::Utc::now();
                let formatted = now.format("%Y%m%dT%H%M%SZ").to_string();
                let bytes = formatted.as_bytes();
                if bytes.len() > buf_len as usize {
                    return -1;
                }
                if write_guest_bytes(
                    &mut caller,
                    &memory,
                    buf_ptr,
                    bytes,
                    "host_get_time",
                    "time",
                )
                .is_err()
                {
                    return -1;
                }
                bytes.len() as i32
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_get_time: {e}")))?;

    // host_get_time_millis() -> i64
    // Returns current UTC time as milliseconds since Unix epoch.
    // Used by WASM modules that need elapsed-time tracking (e.g., resource limiters).
    linker
        .func_wrap(
            "env",
            "host_get_time_millis",
            |_caller: Caller<'_, HostState>| -> i64 { chrono::Utc::now().timestamp_millis() },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_get_time_millis: {e}")))?;

    // host_evaluate_spec(ioa_ptr, ioa_len, state_ptr, state_len,
    //                    action_ptr, action_len, params_ptr, params_len,
    //                    result_buf_ptr, result_buf_len) -> i32
    // Evaluates a single transition against an IOA spec on the host side.
    // Returns: bytes written to result_buf (JSON), or -1 on error, -2 if buf too small.
    #[allow(clippy::too_many_arguments)]
    linker
        .func_wrap(
            "env",
            "host_evaluate_spec",
            |mut caller: Caller<'_, HostState>,
             ioa_ptr: i32,
             ioa_len: i32,
             state_ptr: i32,
             state_len: i32,
             action_ptr: i32,
             action_len: i32,
             params_ptr: i32,
             params_len: i32,
             result_buf_ptr: i32,
             result_buf_len: i32|
             -> i32 {
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let Some(memory) = memory else {
                    return -1;
                };

                // ARN-226: bounds-checked reads before allocating.
                let Ok(ioa_source) = read_guest_lossy(
                    &caller,
                    &memory,
                    ioa_ptr,
                    ioa_len,
                    "host_evaluate_spec",
                    "ioa_source",
                ) else {
                    return -1;
                };
                let Ok(current_state) = read_guest_lossy(
                    &caller,
                    &memory,
                    state_ptr,
                    state_len,
                    "host_evaluate_spec",
                    "current_state",
                ) else {
                    return -1;
                };
                let Ok(action) = read_guest_lossy(
                    &caller,
                    &memory,
                    action_ptr,
                    action_len,
                    "host_evaluate_spec",
                    "action",
                ) else {
                    return -1;
                };

                // Read params JSON (ARN-226: bounds-checked read before allocating).
                let params_json = if params_len > 0 {
                    match read_guest_lossy(
                        &caller,
                        &memory,
                        params_ptr,
                        params_len,
                        "host_evaluate_spec",
                        "params_json",
                    ) {
                        Ok(s) => s,
                        Err(()) => return -1,
                    }
                } else {
                    "{}".to_string()
                };

                // Call host evaluate_spec (synchronous — no async bridge needed)
                let _guest_span = caller.data().guest_spans.enter_active();
                let result_json = match caller.data().host.evaluate_spec(
                    &ioa_source,
                    &current_state,
                    &action,
                    &params_json,
                ) {
                    Ok(json) => json,
                    Err(e) => {
                        format!(r#"{{"success": false, "error": "{e}"}}"#)
                    }
                };

                let result_bytes = result_json.as_bytes();
                if result_bytes.len() > result_buf_len as usize {
                    return -2; // buffer too small
                }
                if memory
                    .write(&mut caller, result_buf_ptr as usize, result_bytes)
                    .is_err()
                {
                    return -1;
                }
                result_bytes.len() as i32
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_evaluate_spec: {e}")))?;

    // --- ADR-0057 streaming primitive FFI (outbound, Phase 1) ---
    //
    // Five imports give WASM guests access to the bidirectional
    // streaming channels maintained by WasmHost::http_streams.
    // Handle IDs are opaque u32s packed into i32 return values.
    //
    // Return code convention for byte-returning functions:
    //   >= 0   : bytes read / written
    //   -1     : WouldBlock
    //   -2     : Closed
    //   -3     : InvalidHandle
    //   -4     : other error (Aborted, network fault)

    // host_http_stream_begin_outbound(method_ptr, method_len,
    //                                  url_ptr, url_len,
    //                                  headers_ptr, headers_len,
    //                                  out_req_handle_ptr,
    //                                  out_resp_handle_ptr) -> i32
    // Returns 0 on success (handles written at out_*_handle_ptr),
    // negative on error per the convention above.
    #[allow(clippy::too_many_arguments)]
    linker
        .func_wrap(
            "env",
            "host_http_stream_begin_outbound",
            |mut caller: Caller<'_, HostState>,
             method_ptr: i32,
             method_len: i32,
             url_ptr: i32,
             url_len: i32,
             headers_ptr: i32,
             headers_len: i32,
             out_req_handle_ptr: i32,
             out_resp_handle_ptr: i32|
             -> i32 {
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let Some(memory) = memory else { return -4 };

                let Ok(method) = read_guest_lossy(
                    &caller,
                    &memory,
                    method_ptr,
                    method_len,
                    "host_http_stream_begin_outbound",
                    "method",
                ) else {
                    return -4;
                };

                let Ok(url) = read_guest_lossy(
                    &caller,
                    &memory,
                    url_ptr,
                    url_len,
                    "host_http_stream_begin_outbound",
                    "url",
                ) else {
                    return -4;
                };

                let headers: Vec<(String, String)> = if headers_len > 0 {
                    let Ok(hdr_buf) = read_guest_bytes(
                        &caller,
                        &memory,
                        headers_ptr,
                        headers_len,
                        "host_http_stream_begin_outbound",
                        "headers",
                    ) else {
                        return -4;
                    };
                    let Ok(headers) =
                        parse_guest_headers(&hdr_buf, "host_http_stream_begin_outbound")
                    else {
                        return -4;
                    };
                    headers
                } else {
                    Vec::new()
                };

                let _guest_span = caller.data().guest_spans.enter_active();
                let host = caller.data().host.clone();
                let head = crate::http_stream::HttpRequestHead {
                    method,
                    url,
                    headers,
                };
                let method_for_log = head.method.clone();
                let url_for_log = redact_url_for_log(&head.url);
                let deadline = caller.data().remaining_host_call_timeout();
                let Ok(result) = run_host_call_with_timeout(
                    "host_http_stream_begin_outbound",
                    deadline,
                    async move { host.http_stream_begin_outbound(head).await },
                ) else {
                    return -4;
                };

                let handles = match result {
                    Ok(h) => h,
                    Err(err) => {
                        tracing::warn!(
                            http_method = %method_for_log,
                            url = %url_for_log,
                            error = %err,
                            "WASM outbound HTTP stream begin failed"
                        );
                        return -4;
                    }
                };

                let req_bytes = handles.request_body.0.to_le_bytes();
                let resp_bytes = handles.response_body.0.to_le_bytes();
                if memory
                    .write(&mut caller, out_req_handle_ptr as usize, &req_bytes)
                    .is_err()
                {
                    return -4;
                }
                if memory
                    .write(&mut caller, out_resp_handle_ptr as usize, &resp_bytes)
                    .is_err()
                {
                    return -4;
                }
                0
            },
        )
        .map_err(|e| {
            WasmError::Compilation(format!(
                "failed to link host_http_stream_begin_outbound: {e}"
            ))
        })?;

    // host_http_stream_read(handle, buf_ptr, buf_cap) -> i32
    // Blocks until a chunk is available. 0 means clean EOF.
    linker
        .func_wrap(
            "env",
            "host_http_stream_read",
            |mut caller: Caller<'_, HostState>, handle: i32, buf_ptr: i32, buf_cap: i32| -> i32 {
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let Some(memory) = memory else { return -4 };
                if buf_cap <= 0 {
                    return -4;
                }

                let _guest_span = caller.data().guest_spans.enter_active();
                let host = caller.data().host.clone();
                let sh = crate::http_stream::StreamHandle(handle as u32);
                let deadline = caller.data().remaining_host_call_timeout();
                let Ok(result) =
                    run_host_call_with_timeout("host_http_stream_read", deadline, async move {
                        host.http_stream_read_bounded(sh, buf_cap as usize).await
                    })
                else {
                    return -4;
                };
                match result {
                    Ok(chunk) => {
                        if chunk.is_empty() {
                            return 0;
                        }
                        if memory.write(&mut caller, buf_ptr as usize, &chunk).is_err() {
                            return -4;
                        }
                        chunk.len() as i32
                    }
                    Err(crate::http_stream::StreamError::WouldBlock) => -1,
                    Err(crate::http_stream::StreamError::Closed) => -2,
                    Err(crate::http_stream::StreamError::InvalidHandle) => -3,
                    Err(crate::http_stream::StreamError::Aborted(reason)) => {
                        tracing::warn!(
                            stream_handle = handle,
                            error = %reason,
                            "WASM HTTP stream read aborted"
                        );
                        -4
                    }
                }
            },
        )
        .map_err(|e| {
            WasmError::Compilation(format!("failed to link host_http_stream_read: {e}"))
        })?;

    // host_http_stream_try_write(handle, data_ptr, data_len) -> i32
    // Non-blocking — returns -1 (WouldBlock) when channel is full.
    linker
        .func_wrap(
            "env",
            "host_http_stream_try_write",
            |mut caller: Caller<'_, HostState>, handle: i32, data_ptr: i32, data_len: i32| -> i32 {
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let Some(memory) = memory else { return -4 };

                let Ok(buf) = read_guest_bytes(
                    &caller,
                    &memory,
                    data_ptr,
                    data_len,
                    "host_http_stream_try_write",
                    "data",
                ) else {
                    return -4;
                };

                let _guest_span = caller.data().guest_spans.enter_active();
                let host = caller.data().host.clone();
                let sh = crate::http_stream::StreamHandle(handle as u32);
                let deadline = caller.data().remaining_host_call_timeout();
                let Ok(result) = run_host_call_with_timeout(
                    "host_http_stream_try_write",
                    deadline,
                    async move { host.http_stream_try_write(sh, buf).await },
                ) else {
                    return -4;
                };
                match result {
                    Ok(n) => n as i32,
                    Err(crate::http_stream::StreamError::WouldBlock) => -1,
                    Err(crate::http_stream::StreamError::Closed) => -2,
                    Err(crate::http_stream::StreamError::InvalidHandle) => -3,
                    Err(crate::http_stream::StreamError::Aborted(reason)) => {
                        tracing::warn!(
                            stream_handle = handle,
                            error = %reason,
                            "WASM HTTP stream write aborted"
                        );
                        -4
                    }
                }
            },
        )
        .map_err(|e| {
            WasmError::Compilation(format!("failed to link host_http_stream_try_write: {e}"))
        })?;

    // host_http_stream_close(handle) -> i32
    linker
        .func_wrap(
            "env",
            "host_http_stream_close",
            |caller: Caller<'_, HostState>, handle: i32| -> i32 {
                let _guest_span = caller.data().guest_spans.enter_active();
                let host = caller.data().host.clone();
                let sh = crate::http_stream::StreamHandle(handle as u32);
                let deadline = caller.data().remaining_host_call_timeout();
                let Ok(result) =
                    run_host_call_with_timeout("host_http_stream_close", deadline, async move {
                        host.http_stream_close(sh).await
                    })
                else {
                    return -4;
                };
                match result {
                    Ok(()) => 0,
                    Err(crate::http_stream::StreamError::InvalidHandle) => -3,
                    Err(crate::http_stream::StreamError::WouldBlock) => {
                        tracing::warn!(
                            stream_handle = handle,
                            "WASM HTTP stream close unexpectedly returned WouldBlock"
                        );
                        -4
                    }
                    Err(crate::http_stream::StreamError::Closed) => {
                        tracing::warn!(
                            stream_handle = handle,
                            "WASM HTTP stream close saw already closed handle"
                        );
                        -4
                    }
                    Err(crate::http_stream::StreamError::Aborted(reason)) => {
                        tracing::warn!(
                            stream_handle = handle,
                            error = %reason,
                            "WASM HTTP stream close aborted"
                        );
                        -4
                    }
                }
            },
        )
        .map_err(|e| {
            WasmError::Compilation(format!("failed to link host_http_stream_close: {e}"))
        })?;

    // host_http_stream_response_head(resp_handle, buf_ptr, buf_cap) -> i32
    // Blocks until the response head is available. Writes JSON
    // `{"status":N,"headers":[["k","v"]...]}` to buf_ptr up to
    // buf_cap bytes. Returns bytes written, -2 if buf too small,
    // -4 on other error.
    linker
        .func_wrap(
            "env",
            "host_http_stream_response_head",
            |mut caller: Caller<'_, HostState>,
             resp_handle: i32,
             buf_ptr: i32,
             buf_cap: i32|
             -> i32 {
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let Some(memory) = memory else { return -4 };

                let _guest_span = caller.data().guest_spans.enter_active();
                let host = caller.data().host.clone();
                let sh = crate::http_stream::StreamHandle(resp_handle as u32);
                let deadline = caller.data().remaining_host_call_timeout();
                let Ok(result) = run_host_call_with_timeout(
                    "host_http_stream_response_head",
                    deadline,
                    async move { host.http_stream_response_head(sh).await },
                ) else {
                    return -4;
                };
                let head = match result {
                    Ok(h) => h,
                    Err(err) => {
                        tracing::warn!(
                            stream_handle = resp_handle,
                            error = %err,
                            "WASM HTTP stream response head failed"
                        );
                        return -4;
                    }
                };
                let encoded = serde_json::json!({
                    "status": head.status,
                    "headers": head.headers,
                });
                let encoded_bytes = encoded.to_string().into_bytes();
                if encoded_bytes.len() > buf_cap as usize {
                    return -2;
                }
                if memory
                    .write(&mut caller, buf_ptr as usize, &encoded_bytes)
                    .is_err()
                {
                    return -4;
                }
                encoded_bytes.len() as i32
            },
        )
        .map_err(|e| {
            WasmError::Compilation(format!(
                "failed to link host_http_stream_response_head: {e}"
            ))
        })?;

    // host_http_stream_send_response_head(resp_handle, head_ptr, head_len) -> i32
    // Inbound-dispatch counterpart. Guest calls this once per
    // invocation to hand the kernel the HTTP response head
    // (status + headers as JSON). Returns 0 on success, negative
    // on error per the stream convention.
    linker
        .func_wrap(
            "env",
            "host_http_stream_send_response_head",
            |mut caller: Caller<'_, HostState>,
             resp_handle: i32,
             head_ptr: i32,
             head_len: i32|
             -> i32 {
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let Some(memory) = memory else { return -4 };

                // ARN-226: bounds-checked read before allocating.
                let Ok(buf) = read_guest_bytes(
                    &caller,
                    &memory,
                    head_ptr,
                    head_len,
                    "host_http_stream_send_response_head",
                    "head",
                ) else {
                    return -4;
                };
                #[derive(serde::Deserialize)]
                struct RawHead {
                    status: u16,
                    #[serde(default)]
                    headers: Vec<(String, String)>,
                }
                let raw: RawHead = match serde_json::from_slice(&buf) {
                    Ok(r) => r,
                    Err(_) => return -4,
                };
                let head = crate::http_stream::HttpResponseHead {
                    status: raw.status,
                    headers: raw.headers,
                };
                let _guest_span = caller.data().guest_spans.enter_active();
                let host = caller.data().host.clone();
                let sh = crate::http_stream::StreamHandle(resp_handle as u32);
                let deadline = caller.data().remaining_host_call_timeout();
                let Ok(result) = run_host_call_with_timeout(
                    "host_http_stream_send_response_head",
                    deadline,
                    async move { host.http_stream_send_response_head(sh, head).await },
                ) else {
                    return -4;
                };
                match result {
                    Ok(()) => 0,
                    Err(crate::http_stream::StreamError::InvalidHandle) => -3,
                    Err(crate::http_stream::StreamError::WouldBlock) => {
                        tracing::warn!(
                            stream_handle = resp_handle,
                            "WASM HTTP stream send response head unexpectedly returned WouldBlock"
                        );
                        -4
                    }
                    Err(crate::http_stream::StreamError::Closed) => {
                        tracing::warn!(
                            stream_handle = resp_handle,
                            "WASM HTTP stream send response head saw closed handle"
                        );
                        -4
                    }
                    Err(crate::http_stream::StreamError::Aborted(reason)) => {
                        tracing::warn!(
                            stream_handle = resp_handle,
                            error = %reason,
                            "WASM HTTP stream send response head aborted"
                        );
                        -4
                    }
                }
            },
        )
        .map_err(|e| {
            WasmError::Compilation(format!(
                "failed to link host_http_stream_send_response_head: {e}"
            ))
        })?;

    super::data_host_functions::link(linker)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_read_bounds_ok_rejects_out_of_bounds_and_overflow() {
        // ARN-226: a guest-supplied `len` must be validated against the guest memory
        // size BEFORE a read buffer is allocated. Out-of-bounds or overflowing
        // `(ptr, len)` ranges must be rejected so a huge `len` can't drive a large
        // host allocation.
        let mem = 64 * 1024; // 64 KiB guest linear memory
        assert!(
            !guest_read_bounds_ok(mem, 0, i32::MAX as usize),
            "an i32::MAX len must be rejected before allocating ~2 GiB"
        );
        assert!(
            !guest_read_bounds_ok(mem, 0, mem + 1),
            "a len past the end of memory must be rejected"
        );
        assert!(
            !guest_read_bounds_ok(mem, mem, 1),
            "a read starting at the end of memory must be rejected"
        );
        assert!(
            !guest_read_bounds_ok(mem, usize::MAX, 1),
            "a ptr + len that overflows usize must be rejected"
        );
        // Legitimate in-bounds reads are still allowed.
        assert!(
            guest_read_bounds_ok(mem, 0, mem),
            "a full in-bounds read is allowed"
        );
        assert!(guest_read_bounds_ok(mem, 0, 0), "an empty read is allowed");
        assert!(
            guest_read_bounds_ok(mem, 100, 200),
            "an interior read is allowed"
        );
    }

    fn ctx_json_with_fields(fields: serde_json::Value) -> String {
        serde_json::json!({
            "entity_state": { "fields": fields }
        })
        .to_string()
    }

    #[test]
    fn resolve_missing_field_returns_not_found() {
        let ctx = ctx_json_with_fields(serde_json::json!({ "other": "value" }));
        let cache = BTreeMap::new();
        assert_eq!(
            resolve_field_bytes(&ctx, &cache, "missing"),
            FieldResolution::NotFound
        );
    }

    #[test]
    fn resolve_plain_string_returns_utf8_bytes() {
        let ctx = ctx_json_with_fields(serde_json::json!({ "message": "hello world" }));
        let cache = BTreeMap::new();
        assert_eq!(
            resolve_field_bytes(&ctx, &cache, "message"),
            FieldResolution::Bytes(b"hello world".to_vec())
        );
    }

    #[test]
    fn resolve_null_returns_empty_bytes() {
        let ctx = ctx_json_with_fields(serde_json::json!({ "thing": null }));
        let cache = BTreeMap::new();
        assert_eq!(
            resolve_field_bytes(&ctx, &cache, "thing"),
            FieldResolution::Bytes(Vec::new())
        );
    }

    #[test]
    fn resolve_object_returns_json_bytes() {
        let ctx = ctx_json_with_fields(serde_json::json!({ "obj": { "k": "v" } }));
        let cache = BTreeMap::new();
        let resolved = resolve_field_bytes(&ctx, &cache, "obj");
        match resolved {
            FieldResolution::Bytes(bytes) => {
                let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("valid JSON");
                assert_eq!(parsed, serde_json::json!({ "k": "v" }));
            }
            other => panic!("expected Bytes, got {other:?}"),
        }
    }

    #[test]
    fn resolve_blob_ref_uses_blob_cache() {
        let blob_key = "field-overflow/sha256/abc.json";
        let ctx = ctx_json_with_fields(serde_json::json!({
            "big_field": {
                "__temper_blob_ref": blob_key,
                "__temper_blob_size": 1024,
                "__temper_blob_encoding": "json",
            }
        }));
        let mut cache = BTreeMap::new();
        let payload = br#""the big value""#.to_vec();
        cache.insert(blob_key.to_string(), payload.clone());

        assert_eq!(
            resolve_field_bytes(&ctx, &cache, "big_field"),
            FieldResolution::Bytes(payload)
        );
    }

    #[test]
    fn resolve_blob_ref_missing_cache_entry() {
        let ctx = ctx_json_with_fields(serde_json::json!({
            "big_field": {
                "__temper_blob_ref": "field-overflow/sha256/absent.json",
                "__temper_blob_size": 2048,
            }
        }));
        let cache = BTreeMap::new();

        match resolve_field_bytes(&ctx, &cache, "big_field") {
            FieldResolution::BlobRefMissing { key } => {
                assert!(key.ends_with("absent.json"));
            }
            other => panic!("expected BlobRefMissing, got {other:?}"),
        }
    }

    #[test]
    fn resolve_malformed_context_returns_host_error() {
        let cache = BTreeMap::new();
        assert_eq!(
            resolve_field_bytes("not json", &cache, "anything"),
            FieldResolution::HostError
        );
    }

    // ── run_host_call_with_timeout tests ─────────────────────────────────
    //
    // These tests exercise `run_host_call_with_timeout_impl` directly so we
    // can supply a short deadline. Paused-time testing is incompatible with
    // this code path because `block_in_place` requires a multi-threaded
    // runtime while `start_paused` requires `current_thread`.

    /// Fast futures complete normally and the returned value is passed through.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_host_call_returns_fast_future_result() {
        let result =
            run_host_call_with_timeout_impl("test_fast", Duration::from_secs(5), async { 42i32 });
        assert_eq!(result, Ok(42));
    }

    /// A future whose completion is driven by tokio tasks (simulating the real
    /// reqwest pattern) still completes through the wrapper: the wrapper must
    /// not deadlock against the runtime that owns it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_host_call_survives_runtime_spawned_work() {
        let result =
            run_host_call_with_timeout_impl("test_spawned", Duration::from_secs(5), async {
                let handle = tokio::spawn(async { "ok" });
                handle.await.unwrap_or("err")
            });
        assert_eq!(result, Ok("ok"));
    }

    /// A hanging future must produce a timeout error within the outer deadline,
    /// not hang forever. This is the core regression guard: prior to this fix,
    /// a hung `http_call` could hold an entity actor unresponsive until the
    /// 5-minute passivation timer fired.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_host_call_times_out_on_hung_future() {
        let start = std::time::Instant::now();
        let result = run_host_call_with_timeout_impl(
            "test_hang",
            Duration::from_millis(100),
            std::future::pending::<()>(),
        );
        let elapsed = start.elapsed();

        assert_eq!(result, Err(()), "Hung future must time out");
        assert!(
            elapsed < Duration::from_secs(2),
            "Timeout must fire within a small multiple of the deadline (got {elapsed:?})"
        );
    }

    /// Default public entrypoint resolves fast futures without touching the
    /// production-length deadline.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_host_call_public_wrapper_passes_fast_futures() {
        let result =
            run_host_call_with_timeout("test_public_fast", Duration::from_secs(5), async {
                "hello"
            });
        assert_eq!(result, Ok("hello"));
    }
}
