//! Generic HTTP fetch WASM module for Temper integrations.
//!
//! Reads URL, method, and headers from `integration_config` in the invocation
//! context, appends `trigger_params` as query parameters (GET) or JSON body
//! (POST), calls `host_http_call`, and returns the response via `host_set_result`.
//!
//! Build: `cargo build -p http-fetch-module --target wasm32-unknown-unknown --release`

use core::ptr::addr_of;

// ---- Host function imports ----

unsafe extern "C" {
    fn host_log(level_ptr: i32, level_len: i32, msg_ptr: i32, msg_len: i32);
    fn host_get_context(buf_ptr: i32, buf_len: i32) -> i32;
    fn host_set_result(ptr: i32, len: i32);
    fn host_http_call(
        method_ptr: i32,
        method_len: i32,
        url_ptr: i32,
        url_len: i32,
        headers_ptr: i32,
        headers_len: i32,
        body_ptr: i32,
        body_len: i32,
        result_buf_ptr: i32,
        result_buf_len: i32,
    ) -> i32;
}

// ---- Buffers ----

const CTX_BUF_LEN: usize = 8192;
const HTTP_BUF_LEN: usize = 65536;

static mut CTX_BUF: [u8; CTX_BUF_LEN] = [0u8; CTX_BUF_LEN];
static mut HTTP_BUF: [u8; HTTP_BUF_LEN] = [0u8; HTTP_BUF_LEN];

// ---- Guest entry point ----

#[unsafe(no_mangle)]
pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
    log("info", "http-fetch: run() called");

    let ctx_json = match read_context() {
        Some(s) => s,
        None => {
            set_error_result("failed to read invocation context");
            return 0;
        }
    };

    // Extract integration_config and trigger_params from the context JSON.
    // We do minimal JSON parsing without a JSON library (wasm32 no_std-friendly).
    let url = match extract_string(&ctx_json, "integration_config", "url") {
        Some(u) => u,
        None => {
            set_error_result("integration_config missing 'url' key");
            return 0;
        }
    };

    let method = extract_string(&ctx_json, "integration_config", "method")
        .unwrap_or_else(|| "GET".to_string());

    let headers_json = extract_string(&ctx_json, "integration_config", "headers")
        .unwrap_or_default();

    // Extract trigger_params as a JSON substring for use as query/body.
    let trigger_params = extract_object(&ctx_json, "trigger_params")
        .unwrap_or_else(|| "{}".to_string());

    // Build the final URL/body depending on method.
    let (final_url, body) = if method == "GET" || method == "get" {
        let qs = params_to_query_string(&trigger_params);
        let sep = if url.contains('?') { "&" } else { "?" };
        if qs.is_empty() {
            (url, String::new())
        } else {
            (format!("{url}{sep}{qs}"), String::new())
        }
    } else {
        (url, trigger_params)
    };

    log("info", &format!("http-fetch: {method} {final_url}"));

    // Make HTTP call via host.
    let response = http_call(&method, &final_url, &headers_json, &body);
    match response {
        Some(resp) => {
            log("info", &format!("http-fetch: response length = {}", resp.len()));

            // Parse status code from response (format: "status_code\nbody")
            let (status_code, resp_body) = match resp.find('\n') {
                Some(pos) => {
                    let code = &resp[..pos];
                    let body = &resp[pos + 1..];
                    (code.to_string(), body.to_string())
                }
                None => ("0".to_string(), resp),
            };

            let result = format!(
                r#"{{"action":"callback","params":{{"status_code":{},"response":{}}},"success":true}}"#,
                json_string_value(&status_code),
                json_string_value(&resp_body),
            );
            set_result(&result);
            0
        }
        None => {
            set_error_result(&format!(
                "HTTP call failed: {method} {final_url} — no response from host"
            ));
            0
        }
    }
}

// ---- Helpers ----

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
        r#"{{"action":"callback","params":{{"error":"{}"}},"success":false,"error":"{}"}}"#,
        escape_json(error),
        escape_json(error),
    );
    set_result(&result);
}

fn http_call(method: &str, url: &str, headers_json: &str, body: &str) -> Option<String> {
    unsafe {
        let ptr = addr_of!(HTTP_BUF) as *const u8;
        let len = host_http_call(
            method.as_ptr() as i32,
            method.len() as i32,
            url.as_ptr() as i32,
            url.len() as i32,
            headers_json.as_ptr() as i32,
            headers_json.len() as i32,
            body.as_ptr() as i32,
            body.len() as i32,
            ptr as i32,
            HTTP_BUF_LEN as i32,
        );
        if len <= 0 {
            return None;
        }
        let slice = core::slice::from_raw_parts(ptr, len as usize);
        Some(String::from_utf8_lossy(slice).to_string())
    }
}

/// Minimal JSON key extraction from a nested object.
/// Looks for `"outer_key":{..."inner_key":"value"...}` and returns value.
fn extract_string(json: &str, outer_key: &str, inner_key: &str) -> Option<String> {
    // Find the outer object
    let outer_pattern = format!(r#""{}""#, outer_key);
    let outer_pos = json.find(&outer_pattern)?;
    let after_outer = &json[outer_pos + outer_pattern.len()..];

    // Find the opening brace
    let brace_pos = after_outer.find('{')?;
    let inner_json = &after_outer[brace_pos..];

    // Find the inner key
    let inner_pattern = format!(r#""{}""#, inner_key);
    let inner_pos = inner_json.find(&inner_pattern)?;
    let after_inner = &inner_json[inner_pos + inner_pattern.len()..];

    // Skip colon and whitespace
    let after_colon = after_inner.trim_start().strip_prefix(':')?;
    let trimmed = after_colon.trim_start();

    // Extract string value
    if trimmed.starts_with('"') {
        let content = &trimmed[1..];
        let mut end = 0;
        let mut escaped = false;
        for (i, c) in content.char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            if c == '\\' {
                escaped = true;
                continue;
            }
            if c == '"' {
                end = i;
                break;
            }
        }
        Some(content[..end].to_string())
    } else {
        None
    }
}

/// Extract a JSON object/value substring for a top-level key.
fn extract_object(json: &str, key: &str) -> Option<String> {
    let pattern = format!(r#""{}""#, key);
    let pos = json.find(&pattern)?;
    let after = &json[pos + pattern.len()..];
    let after_colon = after.trim_start().strip_prefix(':')?;
    let trimmed = after_colon.trim_start();

    if trimmed.starts_with('{') {
        // Find matching closing brace
        let mut depth = 0;
        let mut in_string = false;
        let mut escaped = false;
        for (i, c) in trimmed.char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            if c == '\\' && in_string {
                escaped = true;
                continue;
            }
            if c == '"' {
                in_string = !in_string;
                continue;
            }
            if !in_string {
                if c == '{' {
                    depth += 1;
                } else if c == '}' {
                    depth -= 1;
                    if depth == 0 {
                        return Some(trimmed[..=i].to_string());
                    }
                }
            }
        }
        None
    } else if trimmed.starts_with("null") {
        Some("{}".to_string())
    } else {
        // For non-object values, take until comma or closing brace
        let end = trimmed.find([',', '}'].as_ref()).unwrap_or(trimmed.len());
        Some(trimmed[..end].trim().to_string())
    }
}

/// Convert a flat JSON object `{"key":"val",...}` to query string `key=val&...`.
fn params_to_query_string(json: &str) -> String {
    let trimmed = json.trim();
    if trimmed == "{}" || trimmed == "null" || trimmed.is_empty() {
        return String::new();
    }

    let mut result = String::new();
    // Simple extraction of "key":"value" pairs
    let inner = if trimmed.starts_with('{') && trimmed.ends_with('}') {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };

    let mut pos = 0;
    let bytes = inner.as_bytes();
    while pos < bytes.len() {
        // Skip whitespace
        while pos < bytes.len() && (bytes[pos] == b' ' || bytes[pos] == b',') {
            pos += 1;
        }
        if pos >= bytes.len() {
            break;
        }

        // Extract key
        if bytes[pos] != b'"' {
            break;
        }
        pos += 1;
        let key_start = pos;
        while pos < bytes.len() && bytes[pos] != b'"' {
            pos += 1;
        }
        let key = &inner[key_start..pos];
        pos += 1; // skip closing quote

        // Skip colon
        while pos < bytes.len() && (bytes[pos] == b' ' || bytes[pos] == b':') {
            pos += 1;
        }

        // Extract value
        let value;
        if pos < bytes.len() && bytes[pos] == b'"' {
            pos += 1;
            let val_start = pos;
            while pos < bytes.len() && bytes[pos] != b'"' {
                if bytes[pos] == b'\\' {
                    pos += 1; // skip escaped char
                }
                pos += 1;
            }
            value = inner[val_start..pos].to_string();
            pos += 1; // skip closing quote
        } else {
            let val_start = pos;
            while pos < bytes.len() && bytes[pos] != b',' && bytes[pos] != b'}' {
                pos += 1;
            }
            value = inner[val_start..pos].trim().to_string();
        }

        if !result.is_empty() {
            result.push('&');
        }
        result.push_str(key);
        result.push('=');
        result.push_str(&value);
    }

    result
}

fn json_string_value(s: &str) -> String {
    // If it's already valid JSON (starts with { or [), return as-is
    let trimmed = s.trim();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return trimmed.to_string();
    }
    // Otherwise wrap as a JSON string
    format!(r#""{}""#, escape_json(s))
}

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
