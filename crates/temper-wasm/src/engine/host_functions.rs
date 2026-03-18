//! Host function linker: registers all `env.*` imports for WASM modules.
//!
//! Each host function bridges WASM linear memory to Rust capabilities
//! (logging, secrets, HTTP, streaming, caching, hashing). Functions are
//! linked once per invocation via a fresh `Linker<HostState>`.

use sha2::{Digest, Sha256};
use wasmtime::{Caller, Linker};

use super::{HostState, WasmError};

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

                // Bridge async -> sync
                let host = caller.data().host.clone();
                let result = tokio::task::block_in_place(|| {
                    // determinism-ok: blocking bridge for WASM host call
                    tokio::runtime::Handle::current()
                        .block_on(host.http_call(&method, &url, &headers, &body))
                });

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

                // Bridge async -> sync
                let host = caller.data().host.clone();
                let result = tokio::task::block_in_place(|| {
                    // determinism-ok: blocking bridge for WASM host call
                    tokio::runtime::Handle::current()
                        .block_on(host.connect_call(&url, &headers, &body))
                });

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

                // Bridge async -> sync for HTTP call with binary body
                let host = caller.data().host.clone();
                let result = tokio::task::block_in_place(|| {
                    // determinism-ok: blocking bridge for WASM host call
                    tokio::runtime::Handle::current().block_on(host.http_call_binary(
                        &method,
                        &url,
                        &headers,
                        &body_bytes,
                    ))
                });

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

    Ok(())
}
