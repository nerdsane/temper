//! Raw FFI declarations for Temper WASM host functions.
//!
//! These match the host functions linked by `temper-wasm::engine::link_host_functions`.
//! SDK users should use the typed wrappers in `context.rs` instead.

/// Buffer size for reading invocation context (256 KB).
///
/// Agent conversation state can grow large (10K+ per turn), so this
/// needs to accommodate multi-turn entities.
pub const CTX_BUF_LEN: usize = 262144;

/// Buffer size for HTTP response data (512 KB).
pub const HTTP_BUF_LEN: usize = 524288;

/// Buffer size for secret values (4 KB).
pub const SECRET_BUF_LEN: usize = 4096;

/// Static buffer for context data.
pub static mut CTX_BUF: [u8; CTX_BUF_LEN] = [0u8; CTX_BUF_LEN];

/// Static buffer for HTTP responses.
pub static mut HTTP_BUF: [u8; HTTP_BUF_LEN] = [0u8; HTTP_BUF_LEN];

/// Static buffer for secret values.
pub static mut SECRET_BUF: [u8; SECRET_BUF_LEN] = [0u8; SECRET_BUF_LEN];

unsafe extern "C" {
    /// Log a message via the host.
    pub fn host_log(level_ptr: i32, level_len: i32, msg_ptr: i32, msg_len: i32);

    /// Read the invocation context JSON into a buffer.
    /// Returns the number of bytes written, or needed size if buffer too small.
    pub fn host_get_context(buf_ptr: i32, buf_len: i32) -> i32;

    /// Set the result JSON for this invocation.
    pub fn host_set_result(ptr: i32, len: i32);

    /// Read a secret value by key.
    /// Returns bytes written, needed size if too small, or -1 on error.
    pub fn host_get_secret(key_ptr: i32, key_len: i32, buf_ptr: i32, buf_len: i32) -> i32;

    /// Make an HTTP call via the host.
    /// Returns bytes written to result_buf (format: "status_code\nbody"),
    /// -1 on error, -2 if buffer too small.
    pub fn host_http_call(
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

    /// Make a Connect protocol server-streaming RPC call via the host.
    /// Returns bytes written to result_buf (JSON array of frame payloads),
    /// -1 on error, -2 if buffer too small.
    pub fn host_connect_call(
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
