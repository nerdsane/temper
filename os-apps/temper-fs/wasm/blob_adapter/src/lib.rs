//! TemperFS blob_adapter — WASM guest module for blob storage operations.
//!
//! Handles auth, hashing, caching, and upload/download orchestration for
//! `$value` endpoints. Bytes never enter WASM memory — they flow through
//! the host's StreamRegistry, referenced by stream IDs.
//!
//! All logic here is **hot-reloadable** by deploying a new `.wasm` binary.
//!
//! Host functions used:
//! - `host_hash_stream`: compute content hash (algorithm chosen here, computed by host)
//! - `host_cache_contains`: check if bytes are cached
//! - `host_cache_to_stream`: copy cached bytes to a stream for response
//! - `host_cache_from_stream`: cache bytes from a stream
//! - `host_http_call_stream`: HTTP with stream-based body/response
//! - `host_get_secret`: read secrets (blob_access_key, blob_secret_key)
//! - `host_get_context`: read invocation context
//! - `host_set_result`: return result to host
//! - `host_log`: structured logging
//!
//! Build: `cargo build --target wasm32-unknown-unknown --release`

use core::ptr::addr_of;

// ---- Host function imports ----

unsafe extern "C" {
    fn host_log(level_ptr: i32, level_len: i32, msg_ptr: i32, msg_len: i32);
    fn host_get_context(buf_ptr: i32, buf_len: i32) -> i32;
    fn host_set_result(ptr: i32, len: i32);
    fn host_get_secret(key_ptr: i32, key_len: i32, buf_ptr: i32, buf_len: i32) -> i32;

    /// HTTP with stream-based body/response. Returns HTTP status code, -1 on error.
    fn host_http_call_stream(
        method_ptr: i32,
        method_len: i32,
        url_ptr: i32,
        url_len: i32,
        headers_ptr: i32,
        headers_len: i32,
        body_stream_id_ptr: i32,
        body_stream_id_len: i32,
        response_stream_id_ptr: i32,
        response_stream_id_len: i32,
    ) -> i32;

    /// Check if bytes are cached. Returns 1 if cached, 0 if not.
    fn host_cache_contains(key_ptr: i32, key_len: i32) -> i32;

    /// Copy cached bytes to a stream. Returns byte count, -1 if not cached.
    fn host_cache_to_stream(
        key_ptr: i32,
        key_len: i32,
        stream_id_ptr: i32,
        stream_id_len: i32,
    ) -> i32;

    /// Cache bytes from a stream. Returns 0 on success, -1 on error.
    fn host_cache_from_stream(
        key_ptr: i32,
        key_len: i32,
        stream_id_ptr: i32,
        stream_id_len: i32,
    ) -> i32;

    /// Compute hash of stream bytes. Returns bytes written, -1 on error.
    fn host_hash_stream(
        stream_id_ptr: i32,
        stream_id_len: i32,
        algorithm_ptr: i32,
        algorithm_len: i32,
        result_buf_ptr: i32,
        result_buf_len: i32,
    ) -> i32;
}

// ---- Buffers ----

const CTX_BUF_LEN: usize = 8192;
const SECRET_BUF_LEN: usize = 1024;
const HASH_BUF_LEN: usize = 256;

static mut CTX_BUF: [u8; CTX_BUF_LEN] = [0u8; CTX_BUF_LEN];
static mut SECRET_BUF: [u8; SECRET_BUF_LEN] = [0u8; SECRET_BUF_LEN];
static mut HASH_BUF: [u8; HASH_BUF_LEN] = [0u8; HASH_BUF_LEN];

// ---- Entry point ----

#[unsafe(no_mangle)]
pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
    let ctx_json = match read_context() {
        Some(s) => s,
        None => {
            set_error_result("failed to read invocation context");
            return 1;
        }
    };

    // Parse operation from trigger_params
    let operation = extract_json_str(&ctx_json, "operation");
    let trigger_action = extract_json_str(&ctx_json, "trigger_action");

    log("info", &format!("blob_adapter: trigger={trigger_action} op={operation}"));

    match operation.as_str() {
        "put" => handle_upload(&ctx_json),
        "get" => handle_download(&ctx_json),
        _ => {
            set_error_result(&format!("unknown operation: {operation}"));
            1
        }
    }
}

// ---- Upload ----

fn handle_upload(ctx_json: &str) -> i32 {
    let stream_id = extract_json_str(ctx_json, "stream_id");
    let size_bytes = extract_json_str(ctx_json, "size_bytes");
    let content_type = extract_json_str(ctx_json, "content_type");

    // 1. Compute content hash — algorithm is hot-reloadable!
    let content_hash = match compute_hash(&stream_id, "sha256") {
        Some(h) => h,
        None => {
            set_error_result("failed to compute content hash");
            return 1;
        }
    };

    log("info", &format!("blob_adapter: hash={content_hash}"));

    // 2. CAS dedup — skip upload if blob already stored
    if cache_contains(&content_hash) {
        log("info", "blob_adapter: CAS cache hit, skipping upload");
        let result = format!(
            r#"{{"action":"StreamUpdated","params":{{"content_hash":"{}","size_bytes":{},"mime_type":"{}"}},"success":true}}"#,
            escape_json(&content_hash),
            size_bytes,
            escape_json(&content_type),
        );
        set_result(&result);
        return 0;
    }

    // 3. Read blob storage credentials
    let endpoint = read_secret_or("blob_endpoint", "https://blob.example.com");
    let bucket = read_secret_or("blob_bucket", "temper-fs");

    // 4. Construct URL (content-addressable: key = hash)
    let url = format!("{endpoint}/{bucket}/{content_hash}");

    // 5. Upload — bytes flow from StreamRegistry via host, never through WASM memory
    let headers_json = "[]"; // Simplified: real impl would compute S3 Sig V4
    let status = call_http_stream("PUT", &url, headers_json, &stream_id, "");

    if status < 200 || status >= 300 {
        set_error_result(&format!("upload failed with HTTP {status}"));
        return 1;
    }

    // 6. Cache for future reads and dedup
    cache_from_stream(&content_hash, &stream_id);

    // 7. Return action + params for server to dispatch
    let result = format!(
        r#"{{"action":"StreamUpdated","params":{{"content_hash":"{}","size_bytes":{},"mime_type":"{}"}},"success":true}}"#,
        escape_json(&content_hash),
        size_bytes,
        escape_json(&content_type),
    );
    set_result(&result);
    0
}

// ---- Download ----

fn handle_download(ctx_json: &str) -> i32 {
    let response_stream_id = extract_json_str(ctx_json, "stream_id");

    // Read content_hash from entity_state.fields.content_hash
    // entity_state is nested: {"fields":{"content_hash":"sha256:..."},...}
    let content_hash = {
        let es = extract_json_object(ctx_json, "entity_state");
        if es.is_empty() {
            String::new()
        } else {
            let fields = extract_json_object(&es, "fields");
            if fields.is_empty() {
                // Fallback: try direct extraction from entity_state
                extract_json_str(&es, "content_hash")
            } else {
                extract_json_str(&fields, "content_hash")
            }
        }
    };
    if content_hash.is_empty() {
        set_error_result("entity has no content_hash");
        return 1;
    }

    // 1. Cache check — skip download if bytes already cached
    if cache_contains(&content_hash) {
        log("info", "blob_adapter: cache hit for download");
        let copied = cache_to_stream(&content_hash, &response_stream_id);
        if copied >= 0 {
            let result = r#"{"success":true}"#;
            set_result(result);
            return 0;
        }
        // Fall through to R2 download if cache_to_stream failed
    }

    // 2. Read blob storage credentials
    let endpoint = read_secret_or("blob_endpoint", "https://blob.example.com");
    let bucket = read_secret_or("blob_bucket", "temper-fs");

    // 3. Construct URL
    let url = format!("{endpoint}/{bucket}/{content_hash}");

    // 4. Download — bytes go to StreamRegistry via host
    let headers_json = "[]";
    let status = call_http_stream("GET", &url, headers_json, "", &response_stream_id);

    if status < 200 || status >= 300 {
        set_error_result(&format!("download failed with HTTP {status}"));
        return 1;
    }

    // 5. Cache for next time
    cache_from_stream(&content_hash, &response_stream_id);

    let result = r#"{"success":true}"#;
    set_result(result);
    0
}

// ---- Host function wrappers ----

fn log(level: &str, msg: &str) {
    unsafe {
        host_log(
            level.as_ptr() as i32,
            level.len() as i32,
            msg.as_ptr() as i32,
            msg.len() as i32,
        );
    }
}

fn read_context() -> Option<String> {
    unsafe {
        let ptr = addr_of!(CTX_BUF) as *const u8;
        let len = host_get_context(ptr as i32, CTX_BUF_LEN as i32);
        if len <= 0 || len as usize > CTX_BUF_LEN {
            return None;
        }
        let slice = core::slice::from_raw_parts(ptr, len as usize);
        Some(String::from_utf8_lossy(slice).to_string())
    }
}

fn set_result(json: &str) {
    unsafe {
        host_set_result(json.as_ptr() as i32, json.len() as i32);
    }
}

fn set_error_result(error: &str) {
    let result = format!(
        r#"{{"success":false,"error":"{}"}}"#,
        escape_json(error),
    );
    set_result(&result);
}

fn compute_hash(stream_id: &str, algorithm: &str) -> Option<String> {
    unsafe {
        let ptr = addr_of!(HASH_BUF) as *const u8;
        let len = host_hash_stream(
            stream_id.as_ptr() as i32,
            stream_id.len() as i32,
            algorithm.as_ptr() as i32,
            algorithm.len() as i32,
            ptr as i32,
            HASH_BUF_LEN as i32,
        );
        if len <= 0 {
            return None;
        }
        let slice = core::slice::from_raw_parts(ptr, len as usize);
        Some(String::from_utf8_lossy(slice).to_string())
    }
}

fn cache_contains(key: &str) -> bool {
    unsafe { host_cache_contains(key.as_ptr() as i32, key.len() as i32) == 1 }
}

fn cache_to_stream(key: &str, stream_id: &str) -> i32 {
    unsafe {
        host_cache_to_stream(
            key.as_ptr() as i32,
            key.len() as i32,
            stream_id.as_ptr() as i32,
            stream_id.len() as i32,
        )
    }
}

fn cache_from_stream(key: &str, stream_id: &str) {
    unsafe {
        host_cache_from_stream(
            key.as_ptr() as i32,
            key.len() as i32,
            stream_id.as_ptr() as i32,
            stream_id.len() as i32,
        );
    }
}

fn call_http_stream(
    method: &str,
    url: &str,
    headers_json: &str,
    body_stream_id: &str,
    response_stream_id: &str,
) -> i32 {
    unsafe {
        host_http_call_stream(
            method.as_ptr() as i32,
            method.len() as i32,
            url.as_ptr() as i32,
            url.len() as i32,
            headers_json.as_ptr() as i32,
            headers_json.len() as i32,
            body_stream_id.as_ptr() as i32,
            body_stream_id.len() as i32,
            response_stream_id.as_ptr() as i32,
            response_stream_id.len() as i32,
        )
    }
}

fn read_secret_or(key: &str, default: &str) -> String {
    unsafe {
        let ptr = addr_of!(SECRET_BUF) as *const u8;
        let len = host_get_secret(
            key.as_ptr() as i32,
            key.len() as i32,
            ptr as i32,
            SECRET_BUF_LEN as i32,
        );
        if len <= 0 {
            return default.to_string();
        }
        let slice = core::slice::from_raw_parts(ptr, len as usize);
        String::from_utf8_lossy(slice).to_string()
    }
}

// ---- Minimal JSON helpers (no serde in WASM guest) ----

/// Extract a string value from JSON (top-level key in trigger_params or context).
fn extract_json_str(json: &str, key: &str) -> String {
    // Look in trigger_params first, then top level
    let search_key = format!(r#""{key}":""#);
    if let Some(start_idx) = json.find(&search_key) {
        let value_start = start_idx + search_key.len();
        if let Some(end_idx) = json[value_start..].find('"') {
            return json[value_start..value_start + end_idx].to_string();
        }
    }
    // Try numeric value
    let search_key_num = format!(r#""{key}":"#);
    if let Some(start_idx) = json.find(&search_key_num) {
        let value_start = start_idx + search_key_num.len();
        let rest = &json[value_start..];
        let end = rest
            .find(|c: char| c == ',' || c == '}' || c == ' ')
            .unwrap_or(rest.len());
        return rest[..end].to_string();
    }
    String::new()
}

/// Extract a JSON object value as a string (brace-matched).
fn extract_json_object(json: &str, key: &str) -> String {
    let search = format!(r#""{key}":"#);
    if let Some(start) = json.find(&search) {
        let rest = &json[start + search.len()..];
        // Skip whitespace
        let rest = rest.trim_start();
        if rest.starts_with('{') {
            // Brace-match to find the end
            let mut depth = 0;
            let mut in_string = false;
            let mut escape_next = false;
            for (i, c) in rest.char_indices() {
                if escape_next {
                    escape_next = false;
                    continue;
                }
                match c {
                    '\\' if in_string => escape_next = true,
                    '"' => in_string = !in_string,
                    '{' if !in_string => depth += 1,
                    '}' if !in_string => {
                        depth -= 1;
                        if depth == 0 {
                            return rest[..=i].to_string();
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    String::new()
}

/// Extract a string value from a nested JSON object.
fn extract_nested_json_str(json: &str, outer_key: &str, inner_key: &str) -> String {
    let outer_search = format!(r#""{outer_key}":{{"#);
    if let Some(outer_start) = json.find(&outer_search) {
        let nested = &json[outer_start + outer_search.len()..];
        return extract_json_str(&format!("{{{nested}"), inner_key);
    }
    // Try without brace directly after colon (could have whitespace)
    let outer_search2 = format!(r#""{outer_key}":"#);
    if let Some(outer_start) = json.find(&outer_search2) {
        let rest = &json[outer_start + outer_search2.len()..];
        return extract_json_str(rest, inner_key);
    }
    String::new()
}

/// Minimal JSON string escaping.
fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out
}
