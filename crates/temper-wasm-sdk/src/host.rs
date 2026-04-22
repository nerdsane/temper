//! Raw FFI declarations for Temper WASM host functions.
//!
//! These match the host functions linked by `temper-wasm::engine::link_host_functions`.
//! SDK users should use the typed wrappers in `context.rs` instead.

/// Buffer size for reading invocation context (512 KB).
///
/// Agent conversation state can grow large (10K+ per turn), so this
/// needs to accommodate multi-turn entities. Increased from 256 KB
/// to handle entities with accumulated adapter/WASM callback fields.
pub const CTX_BUF_LEN: usize = 524288;

/// Buffer size for HTTP response data (4 MB).
///
/// LLM streaming responses (SSE deframed data payloads) can be large,
/// especially when the response includes tool calls with substantial output.
pub const HTTP_BUF_LEN: usize = 4 * 1024 * 1024;

/// Buffer size for secret values (4 KB).
pub const SECRET_BUF_LEN: usize = 4096;

/// Buffer size for spec evaluation results (64 KB).
pub const SPEC_EVAL_BUF_LEN: usize = 65536;

/// Static buffer for spec evaluation results.
pub static mut SPEC_EVAL_BUF: [u8; SPEC_EVAL_BUF_LEN] = [0u8; SPEC_EVAL_BUF_LEN];

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

    /// Emit a replayable progress event for the current entity.
    /// Returns 0 on success, -1 on error.
    pub fn host_emit_progress(ptr: i32, len: i32) -> i32;

    /// Emit a Temper wide event from the guest.
    /// Returns 0 on success, -1 on error.
    pub fn host_emit_wide_event(ptr: i32, len: i32) -> i32;

    /// Emit a structured log event from the guest.
    /// Returns 0 on success, -1 on error.
    pub fn host_log_structured(ptr: i32, len: i32) -> i32;

    /// Emit a metric directly from the guest.
    /// Returns 0 on success, -1 on error.
    pub fn host_emit_metric(ptr: i32, len: i32) -> i32;

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

    /// Get current UTC time as milliseconds since Unix epoch.
    /// Used for elapsed-time tracking in WASM modules (e.g., resource limiters).
    pub fn host_get_time_millis() -> i64;

    /// Evaluate a single transition against an IOA spec on the host.
    /// Returns bytes written to result_buf (JSON), -1 on error, -2 if buf too small.
    pub fn host_evaluate_spec(
        ioa_ptr: i32,
        ioa_len: i32,
        state_ptr: i32,
        state_len: i32,
        action_ptr: i32,
        action_len: i32,
        params_ptr: i32,
        params_len: i32,
        result_buf_ptr: i32,
        result_buf_len: i32,
    ) -> i32;

    /// Read an entity-state field into a buffer, transparently resolving
    /// blob-ref envelopes. Returns bytes written, needed size if > buf_len,
    /// -1 if field not found, -2 if blob ref pre-fetch missing, -3 on error.
    ///
    /// See ADR-0046.
    pub fn host_read_field(
        field_name_ptr: i32,
        field_name_len: i32,
        buf_ptr: i32,
        buf_len: i32,
    ) -> i32;

    // --- ADR-0057 streaming primitive (outbound, Phase 1) ---

    /// Open an outbound streaming HTTP exchange. Writes two u32
    /// handle IDs (little-endian) at `out_req_handle_ptr` and
    /// `out_resp_handle_ptr`. Returns 0 on success, negative on
    /// error (see the stream_read/write error convention).
    ///
    /// Headers are a JSON array of `[key, value]` pairs, matching
    /// `host_http_call`.
    pub fn host_http_stream_begin_outbound(
        method_ptr: i32,
        method_len: i32,
        url_ptr: i32,
        url_len: i32,
        headers_ptr: i32,
        headers_len: i32,
        out_req_handle_ptr: i32,
        out_resp_handle_ptr: i32,
    ) -> i32;

    /// Read the next chunk from a stream handle into `buf_ptr`.
    /// Blocks until a chunk is available or the peer closes.
    /// Returns: positive = bytes read, 0 = clean EOF, -1 = WouldBlock,
    /// -2 = Closed, -3 = InvalidHandle, -4 = other error.
    pub fn host_http_stream_read(handle: i32, buf_ptr: i32, buf_cap: i32) -> i32;

    /// Non-blocking write to a stream handle. Returns:
    /// positive = bytes written, -1 = WouldBlock (retry after yielding),
    /// -2 = Closed, -3 = InvalidHandle, -4 = other error.
    pub fn host_http_stream_try_write(handle: i32, data_ptr: i32, data_len: i32) -> i32;

    /// Close a stream handle, freeing its registry slot.
    /// Returns 0 on success, -3 on InvalidHandle, -4 on other error.
    pub fn host_http_stream_close(handle: i32) -> i32;

    /// Block until the HTTP response head (status + headers) is
    /// available for the given response-body handle. Writes a JSON
    /// object `{"status":N, "headers":[["k","v"],...]}` to
    /// `buf_ptr` up to `buf_cap` bytes.
    /// Returns: positive = bytes written, -2 = buffer too small,
    /// -4 = other error.
    pub fn host_http_stream_response_head(
        resp_handle: i32,
        buf_ptr: i32,
        buf_cap: i32,
    ) -> i32;
}
