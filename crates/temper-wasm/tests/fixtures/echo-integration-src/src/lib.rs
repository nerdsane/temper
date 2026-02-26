//! Echo integration — sample WASM guest module for Temper.
//!
//! Demonstrates the full Temper WASM ABI:
//! - `host_log`: emit structured logs from guest
//! - `host_get_context`: read the invocation context (trigger, entity state)
//! - `host_set_result`: return callback action + params to Temper
//! - `host_http_call`: make an HTTP request via the host
//! - `host_get_secret`: read a secret from the host secret store
//!
//! Build: `cargo build --target wasm32-unknown-unknown --release`

use core::ptr::addr_of;

// ---- Host function imports ----

unsafe extern "C" {
    /// Log a message. level and msg are UTF-8 encoded.
    fn host_log(level_ptr: i32, level_len: i32, msg_ptr: i32, msg_len: i32);

    /// Read invocation context JSON into buf. Returns actual length.
    fn host_get_context(buf_ptr: i32, buf_len: i32) -> i32;

    /// Write result JSON to the host.
    fn host_set_result(ptr: i32, len: i32);

    /// Make an HTTP request. Returns bytes written to result_buf, -1 on error, -2 if buf too small.
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

    /// Read a secret by key. Returns actual length, -1 on error.
    fn host_get_secret(key_ptr: i32, key_len: i32, buf_ptr: i32, buf_len: i32) -> i32;
}

// ---- Buffers (static, wasm32 single-threaded) ----

const CTX_BUF_LEN: usize = 4096;
const HTTP_BUF_LEN: usize = 8192;
const SECRET_BUF_LEN: usize = 1024;

static mut CTX_BUF: [u8; CTX_BUF_LEN] = [0u8; CTX_BUF_LEN];
static mut HTTP_BUF: [u8; HTTP_BUF_LEN] = [0u8; HTTP_BUF_LEN];
static mut SECRET_BUF: [u8; SECRET_BUF_LEN] = [0u8; SECRET_BUF_LEN];

// ---- Guest entry point ----

/// Entry point called by the Temper WASM engine.
///
/// `_ctx_ptr` and `_ctx_len` point to the context JSON already written into
/// guest memory by the engine. We re-read via `host_get_context` to exercise
/// the full ABI.
#[unsafe(no_mangle)]
pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
    // 1. Log that we started
    log("info", "echo-integration: run() called");

    // 2. Read invocation context via host
    let ctx_json = match read_context() {
        Some(s) => s,
        None => {
            set_error_result("failed to read invocation context");
            return 1;
        }
    };

    log(
        "info",
        &format!("echo-integration: context length = {}", ctx_json.len()),
    );

    // 3. Try an HTTP call (GET to a dummy URL — will use SimWasmHost in tests)
    let http_result = http_get("https://echo.example.com/ping");
    let http_status = match &http_result {
        Some(s) => {
            log(
                "info",
                &format!("echo-integration: HTTP response length = {}", s.len()),
            );
            s.clone()
        }
        None => {
            log("warn", "echo-integration: HTTP call returned no data");
            String::from("-1\n")
        }
    };

    // 4. Try reading a secret (best-effort, not required for success)
    let _secret = read_secret("ECHO_API_KEY");

    // 5. Build success result
    let result = format!(
        r#"{{"action":"EchoSucceeded","params":{{"echo_context_len":{},"http_response":"{}"}},"success":true}}"#,
        ctx_json.len(),
        escape_json(&http_status),
    );

    set_result(&result);
    0 // success
}

// ---- Helper functions ----

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
        r#"{{"action":"EchoFailed","params":{{"error":"{}"}},"success":false,"error":"{}"}}"#,
        escape_json(error),
        escape_json(error),
    );
    set_result(&result);
}

fn http_get(url: &str) -> Option<String> {
    let method = "GET";
    unsafe {
        let ptr = addr_of!(HTTP_BUF) as *const u8;
        let len = host_http_call(
            method.as_ptr() as i32,
            method.len() as i32,
            url.as_ptr() as i32,
            url.len() as i32,
            0,
            0, // no headers
            0,
            0, // no body
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

fn read_secret(key: &str) -> Option<String> {
    unsafe {
        let ptr = addr_of!(SECRET_BUF) as *const u8;
        let len = host_get_secret(
            key.as_ptr() as i32,
            key.len() as i32,
            ptr as i32,
            SECRET_BUF_LEN as i32,
        );
        if len <= 0 {
            return None;
        }
        let slice = core::slice::from_raw_parts(ptr, len as usize);
        Some(String::from_utf8_lossy(slice).to_string())
    }
}

/// Minimal JSON string escaping (quotes and backslashes).
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
