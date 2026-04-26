//! Host function linker: registers all `env.*` imports for WASM modules.
//!
//! Each host function bridges WASM linear memory to Rust capabilities
//! (logging, secrets, HTTP, streaming, caching, hashing). Functions are
//! linked once per invocation via a fresh `Linker<HostState>`.

use std::collections::BTreeMap;
use std::future::Future;
use std::time::Duration;

use sha2::{Digest, Sha256};
use wasmtime::{Caller, Linker};

use super::{HostState, WasmError};

/// Outer deadline for each host-side async call invoked from a WASM guest.
///
/// `WasmHost` implementations (notably `ProductionWasmHost` via `reqwest`)
/// carry their own inner timeouts (currently 30 s for HTTP, 10 s for
/// connect), so in the happy path this bound is never reached. The outer
/// wrapper is defensive: if the inner timeout fails to fire — for example
/// when the async host call is starved because the FFI thread is holding
/// the current tokio worker via `block_in_place` — this deadline guarantees
/// the guest always gets a result instead of pinning the entity actor until
/// passivation. Observed in production: a hung `http_call` held an actor
/// unresponsive for 6+ minutes until the passivation timer fired.
const HOST_CALL_OUTER_TIMEOUT: Duration = Duration::from_secs(60);

/// Run an async host-side future from a synchronous WASM FFI context with an
/// outer deadline. See `HOST_CALL_OUTER_TIMEOUT` for rationale.
///
/// Returns `Err(())` on outer timeout (logged via `tracing::warn`); the
/// caller should translate this to the WASM ABI's error sentinel (`-1`).
///
/// The `label` is used in the timeout log line so operators can tell which
/// host function hit the deadline without attaching a debugger.
pub(crate) fn run_host_call_with_timeout<T, F>(label: &'static str, fut: F) -> Result<T, ()>
where
    F: Future<Output = T>,
{
    run_host_call_with_timeout_impl(label, HOST_CALL_OUTER_TIMEOUT, fut)
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
                "WASM host call exceeded outer deadline; returning error to guest"
            );
            Err(())
        }
    }
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
                if let Some(memory) = memory {
                    let mut level_buf = vec![0u8; level_len as usize];
                    let mut msg_buf = vec![0u8; msg_len as usize];
                    let _ = memory.read(&caller, level_ptr as usize, &mut level_buf);
                    let _ = memory.read(&caller, msg_ptr as usize, &mut msg_buf);
                    let level = String::from_utf8_lossy(&level_buf);
                    let msg = String::from_utf8_lossy(&msg_buf);
                    caller.data().host.log(&level, &msg);
                }
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_log: {e}")))?;

    // host_get_context(buf_ptr, buf_len) -> actual_len
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
                if let Some(memory) = memory {
                    let _ = memory.write(&mut caller, buf_ptr as usize, ctx_bytes);
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
                if let Some(memory) = memory {
                    let mut buf = vec![0u8; len as usize];
                    let _ = memory.read(&caller, ptr as usize, &mut buf);
                    if let Ok(s) = String::from_utf8(buf) {
                        caller.data_mut().result_json = Some(s);
                    }
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
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let Some(memory) = memory else {
                    return -1;
                };
                let mut buf = vec![0u8; len as usize];
                if memory.read(&caller, ptr as usize, &mut buf).is_err() {
                    return -1;
                }
                let Ok(payload) = String::from_utf8(buf) else {
                    return -1;
                };
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
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let Some(memory) = memory else {
                    return -1;
                };
                let mut buf = vec![0u8; len as usize];
                if memory.read(&caller, ptr as usize, &mut buf).is_err() {
                    return -1;
                }
                let Ok(payload) = String::from_utf8(buf) else {
                    return -1;
                };
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
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let Some(memory) = memory else {
                    return -1;
                };
                let mut buf = vec![0u8; len as usize];
                if memory.read(&caller, ptr as usize, &mut buf).is_err() {
                    return -1;
                }
                let Ok(payload) = String::from_utf8(buf) else {
                    return -1;
                };
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
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let Some(memory) = memory else {
                    return -1;
                };
                let mut buf = vec![0u8; len as usize];
                if memory.read(&caller, ptr as usize, &mut buf).is_err() {
                    return -1;
                }
                let Ok(payload) = String::from_utf8(buf) else {
                    return -1;
                };
                match caller.data().host.emit_metric(&payload) {
                    Ok(()) => 0,
                    Err(_) => -1,
                }
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_emit_metric: {e}")))?;

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

                let mut key_buf = vec![0u8; key_len as usize];
                let _ = memory.read(&caller, key_ptr as usize, &mut key_buf);
                let key = String::from_utf8_lossy(&key_buf);

                match caller.data().host.get_secret(&key) {
                    Ok(secret) => {
                        let secret_bytes = secret.as_bytes();
                        if (buf_len as usize) < secret_bytes.len() {
                            return secret_bytes.len() as i32;
                        }
                        let _ = memory.write(&mut caller, buf_ptr as usize, secret_bytes);
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
                let mut method_buf = vec![0u8; method_len as usize];
                let _ = memory.read(&caller, method_ptr as usize, &mut method_buf);
                let method = String::from_utf8_lossy(&method_buf).to_string();

                // Read URL
                let mut url_buf = vec![0u8; url_len as usize];
                let _ = memory.read(&caller, url_ptr as usize, &mut url_buf);
                let url = String::from_utf8_lossy(&url_buf).to_string();

                // Read headers (JSON array of [key, value] pairs)
                let headers: Vec<(String, String)> = if headers_len > 0 {
                    let mut hdr_buf = vec![0u8; headers_len as usize];
                    let _ = memory.read(&caller, headers_ptr as usize, &mut hdr_buf);
                    serde_json::from_slice(&hdr_buf).unwrap_or_default()
                } else {
                    vec![]
                };

                // Read body
                let body = if body_len > 0 {
                    let mut body_buf = vec![0u8; body_len as usize];
                    let _ = memory.read(&caller, body_ptr as usize, &mut body_buf);
                    String::from_utf8_lossy(&body_buf).to_string()
                } else {
                    String::new()
                };

                // Bridge async -> sync with an outer deadline so a hanging
                // host call can never pin the actor indefinitely.
                let host = caller.data().host.clone();
                let Ok(result) = run_host_call_with_timeout("host_http_call", async move {
                    host.http_call(&method, &url, &headers, &body).await
                }) else {
                    return -1;
                };

                match result {
                    Ok((status, resp_body)) => {
                        let response = format!("{status}\n{resp_body}");
                        let resp_bytes = response.as_bytes();
                        if resp_bytes.len() > result_buf_len as usize {
                            return -2; // buffer too small
                        }
                        let _ = memory.write(&mut caller, result_buf_ptr as usize, resp_bytes);
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
                    let mut requests_buf = vec![0u8; requests_len as usize];
                    let _ = memory.read(&caller, requests_ptr as usize, &mut requests_buf);
                    match serde_json::from_slice(&requests_buf) {
                        Ok(parsed) => parsed,
                        Err(_) => return -1,
                    }
                } else {
                    Vec::new()
                };

                let host = caller.data().host.clone();
                let result = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(
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
                        ),
                    )
                });

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
                        let _ = memory.write(&mut caller, result_buf_ptr as usize, response_bytes);
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
                let mut url_buf = vec![0u8; url_len as usize];
                let _ = memory.read(&caller, url_ptr as usize, &mut url_buf);
                let url = String::from_utf8_lossy(&url_buf).to_string();

                // Read headers (JSON array of [key, value] pairs)
                let headers: Vec<(String, String)> = if headers_len > 0 {
                    let mut hdr_buf = vec![0u8; headers_len as usize];
                    let _ = memory.read(&caller, headers_ptr as usize, &mut hdr_buf);
                    serde_json::from_slice(&hdr_buf).unwrap_or_default()
                } else {
                    vec![]
                };

                // Read body
                let body = if body_len > 0 {
                    let mut body_buf = vec![0u8; body_len as usize];
                    let _ = memory.read(&caller, body_ptr as usize, &mut body_buf);
                    String::from_utf8_lossy(&body_buf).to_string()
                } else {
                    String::new()
                };

                // Bridge async -> sync with an outer deadline so a hanging
                // host call can never pin the actor indefinitely.
                let host = caller.data().host.clone();
                let Ok(result) = run_host_call_with_timeout("host_connect_call", async move {
                    host.connect_call(&url, &headers, &body).await
                }) else {
                    return -1;
                };

                match result {
                    Ok(frames) => {
                        let json = serde_json::to_string(&frames).unwrap_or_else(|_| "[]".into());
                        let json_bytes = json.as_bytes();
                        if json_bytes.len() > result_buf_len as usize {
                            return -2; // buffer too small
                        }
                        let _ = memory.write(&mut caller, result_buf_ptr as usize, json_bytes);
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
                let mut method_buf = vec![0u8; method_len as usize];
                let _ = memory.read(&caller, method_ptr as usize, &mut method_buf);
                let method = String::from_utf8_lossy(&method_buf).to_string();

                // Read URL
                let mut url_buf = vec![0u8; url_len as usize];
                let _ = memory.read(&caller, url_ptr as usize, &mut url_buf);
                let url = String::from_utf8_lossy(&url_buf).to_string();

                // Read headers (JSON array of [key, value] pairs)
                let headers: Vec<(String, String)> = if headers_len > 0 {
                    let mut hdr_buf = vec![0u8; headers_len as usize];
                    let _ = memory.read(&caller, headers_ptr as usize, &mut hdr_buf);
                    serde_json::from_slice(&hdr_buf).unwrap_or_default()
                } else {
                    vec![]
                };

                // Read body stream ID
                let body_stream_id = if body_stream_id_len > 0 {
                    let mut id_buf = vec![0u8; body_stream_id_len as usize];
                    let _ = memory.read(&caller, body_stream_id_ptr as usize, &mut id_buf);
                    String::from_utf8_lossy(&id_buf).to_string()
                } else {
                    String::new()
                };

                // Read response stream ID
                let response_stream_id = if response_stream_id_len > 0 {
                    let mut id_buf = vec![0u8; response_stream_id_len as usize];
                    let _ = memory.read(&caller, response_stream_id_ptr as usize, &mut id_buf);
                    String::from_utf8_lossy(&id_buf).to_string()
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
                let host = caller.data().host.clone();
                let Ok(result) =
                    run_host_call_with_timeout("host_http_call_stream", async move {
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

                let mut key_buf = vec![0u8; key_len as usize];
                let _ = memory.read(&caller, key_ptr as usize, &mut key_buf);
                let key = String::from_utf8_lossy(&key_buf);

                let streams = caller.data().streams.read().expect("streams lock poisoned"); // ci-ok: infallible lock
                if streams.cache_contains(&key) { 1 } else { 0 }
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

                let mut key_buf = vec![0u8; key_len as usize];
                let _ = memory.read(&caller, key_ptr as usize, &mut key_buf);
                let key = String::from_utf8_lossy(&key_buf).to_string();

                let mut id_buf = vec![0u8; stream_id_len as usize];
                let _ = memory.read(&caller, stream_id_ptr as usize, &mut id_buf);
                let stream_id = String::from_utf8_lossy(&id_buf).to_string();

                let mut streams = caller
                    .data()
                    .streams
                    .write()
                    .expect("streams lock poisoned"); // ci-ok: infallible lock
                match streams.cache_to_stream(&key, &stream_id) {
                    Some(byte_count) => byte_count as i32,
                    None => -1,
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

                let mut name_buf = vec![0u8; field_name_len as usize];
                if memory
                    .read(&caller, field_name_ptr as usize, &mut name_buf)
                    .is_err()
                {
                    return -3;
                }
                let field_name = String::from_utf8_lossy(&name_buf).to_string();

                let bytes = match resolve_field_bytes(
                    &caller.data().context_json,
                    &caller.data().blob_cache,
                    &field_name,
                ) {
                    FieldResolution::Bytes(b) => b,
                    FieldResolution::NotFound => return -1,
                    FieldResolution::BlobRefMissing { key } => {
                        tracing::warn!(
                            field = %field_name,
                            blob_key = %key,
                            "host_read_field: blob ref not in prefetch cache"
                        );
                        return -2;
                    }
                    FieldResolution::HostError => return -3,
                };

                let needed = bytes.len() as i32;
                if needed > buf_len {
                    // Buffer too small — signal needed size; caller retries with larger buf.
                    return needed;
                }
                if memory.write(&mut caller, buf_ptr as usize, &bytes).is_err() {
                    return -3;
                }
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

                let mut key_buf = vec![0u8; key_len as usize];
                let _ = memory.read(&caller, key_ptr as usize, &mut key_buf);
                let key = String::from_utf8_lossy(&key_buf).to_string();

                let mut id_buf = vec![0u8; stream_id_len as usize];
                let _ = memory.read(&caller, stream_id_ptr as usize, &mut id_buf);
                let stream_id = String::from_utf8_lossy(&id_buf).to_string();

                let mut streams = caller
                    .data()
                    .streams
                    .write()
                    .expect("streams lock poisoned"); // ci-ok: infallible lock
                // Read bytes from stream without consuming it
                let bytes = match streams.get_stream(&stream_id) {
                    Some(b) => b.to_vec(),
                    None => return -1,
                };
                streams.cache_put(&key, bytes);
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
                let mut id_buf = vec![0u8; stream_id_len as usize];
                let _ = memory.read(&caller, stream_id_ptr as usize, &mut id_buf);
                let stream_id = String::from_utf8_lossy(&id_buf).to_string();

                // Read algorithm
                let mut algo_buf = vec![0u8; algorithm_len as usize];
                let _ = memory.read(&caller, algorithm_ptr as usize, &mut algo_buf);
                let algorithm = String::from_utf8_lossy(&algo_buf).to_string();

                // Hash stream bytes in-place (no clone)
                let streams = caller.data().streams.read().expect("streams lock poisoned"); // ci-ok: infallible lock
                let Some(bytes) = streams.get_stream(&stream_id) else {
                    return -1;
                };

                let hex_hash = match algorithm.as_str() {
                    "sha256" => {
                        let mut hasher = Sha256::new();
                        hasher.update(bytes);
                        format!("sha256:{:x}", hasher.finalize())
                    }
                    _ => return -1,
                };
                drop(streams);

                // Write hex hash to result buffer
                let hash_bytes = hex_hash.as_bytes();
                if hash_bytes.len() > result_buf_len as usize {
                    return -1; // buffer too small
                }
                let _ = memory.write(&mut caller, result_buf_ptr as usize, hash_bytes);
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
                let _ = memory.write(&mut caller, buf_ptr as usize, bytes);
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

                // Read IOA source
                let mut ioa_buf = vec![0u8; ioa_len as usize];
                if memory
                    .read(&caller, ioa_ptr as usize, &mut ioa_buf)
                    .is_err()
                {
                    return -1;
                }
                let ioa_source = String::from_utf8_lossy(&ioa_buf).to_string();

                // Read current state
                let mut state_buf = vec![0u8; state_len as usize];
                if memory
                    .read(&caller, state_ptr as usize, &mut state_buf)
                    .is_err()
                {
                    return -1;
                }
                let current_state = String::from_utf8_lossy(&state_buf).to_string();

                // Read action
                let mut action_buf = vec![0u8; action_len as usize];
                if memory
                    .read(&caller, action_ptr as usize, &mut action_buf)
                    .is_err()
                {
                    return -1;
                }
                let action = String::from_utf8_lossy(&action_buf).to_string();

                // Read params JSON
                let params_json = if params_len > 0 {
                    let mut params_buf = vec![0u8; params_len as usize];
                    if memory
                        .read(&caller, params_ptr as usize, &mut params_buf)
                        .is_err()
                    {
                        return -1;
                    }
                    String::from_utf8_lossy(&params_buf).to_string()
                } else {
                    "{}".to_string()
                };

                // Call host evaluate_spec (synchronous — no async bridge needed)
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

                let mut method_buf = vec![0u8; method_len as usize];
                let _ = memory.read(&caller, method_ptr as usize, &mut method_buf);
                let method = String::from_utf8_lossy(&method_buf).to_string();

                let mut url_buf = vec![0u8; url_len as usize];
                let _ = memory.read(&caller, url_ptr as usize, &mut url_buf);
                let url = String::from_utf8_lossy(&url_buf).to_string();

                let headers: Vec<(String, String)> = if headers_len > 0 {
                    let mut hdr_buf = vec![0u8; headers_len as usize];
                    let _ = memory.read(&caller, headers_ptr as usize, &mut hdr_buf);
                    serde_json::from_slice(&hdr_buf).unwrap_or_default()
                } else {
                    Vec::new()
                };

                let host = caller.data().host.clone();
                let head = crate::http_stream::HttpRequestHead {
                    method,
                    url,
                    headers,
                };
                let result = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current()
                        .block_on(host.http_stream_begin_outbound(head))
                });

                let handles = match result {
                    Ok(h) => h,
                    Err(_) => return -4,
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

                let host = caller.data().host.clone();
                let sh = crate::http_stream::StreamHandle(handle as u32);
                let result = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current()
                        .block_on(host.http_stream_read_bounded(sh, buf_cap as usize))
                });
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
                    Err(crate::http_stream::StreamError::Aborted(_)) => -4,
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

                let mut buf = vec![0u8; data_len as usize];
                let _ = memory.read(&caller, data_ptr as usize, &mut buf);

                let host = caller.data().host.clone();
                let sh = crate::http_stream::StreamHandle(handle as u32);
                let result = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(host.http_stream_try_write(sh, buf))
                });
                match result {
                    Ok(n) => n as i32,
                    Err(crate::http_stream::StreamError::WouldBlock) => -1,
                    Err(crate::http_stream::StreamError::Closed) => -2,
                    Err(crate::http_stream::StreamError::InvalidHandle) => -3,
                    Err(crate::http_stream::StreamError::Aborted(_)) => -4,
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
                let host = caller.data().host.clone();
                let sh = crate::http_stream::StreamHandle(handle as u32);
                let result = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(host.http_stream_close(sh))
                });
                match result {
                    Ok(()) => 0,
                    Err(crate::http_stream::StreamError::InvalidHandle) => -3,
                    Err(_) => -4,
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

                let host = caller.data().host.clone();
                let sh = crate::http_stream::StreamHandle(resp_handle as u32);
                let result = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(host.http_stream_response_head(sh))
                });
                let head = match result {
                    Ok(h) => h,
                    Err(_) => return -4,
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

                let mut buf = vec![0u8; head_len as usize];
                if memory.read(&caller, head_ptr as usize, &mut buf).is_err() {
                    return -4;
                }
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
                let host = caller.data().host.clone();
                let sh = crate::http_stream::StreamHandle(resp_handle as u32);
                let result = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current()
                        .block_on(host.http_stream_send_response_head(sh, head))
                });
                match result {
                    Ok(()) => 0,
                    Err(crate::http_stream::StreamError::InvalidHandle) => -3,
                    Err(_) => -4,
                }
            },
        )
        .map_err(|e| {
            WasmError::Compilation(format!(
                "failed to link host_http_stream_send_response_head: {e}"
            ))
        })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let result = run_host_call_with_timeout_impl(
            "test_spawned",
            Duration::from_secs(5),
            async {
                let handle = tokio::spawn(async { "ok" });
                handle.await.unwrap_or("err")
            },
        );
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
        let result = run_host_call_with_timeout("test_public_fast", async { "hello" });
        assert_eq!(result, Ok("hello"));
    }
}
