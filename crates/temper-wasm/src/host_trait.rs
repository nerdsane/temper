//! DST-compliant host function trait and implementations.
//!
//! Production uses real HTTP + secret store. Simulation uses canned
//! responses for deterministic testing.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::metrics;
use crate::types::WasmInvocationContext;
use crate::workflow_headers::add_workflow_observability_headers;
use temper_observe::wide_event::{self, EventKind, WideEvent};

mod guest_progress;
mod internal_http;
pub(crate) mod span_hints;

pub use internal_http::{InternalHttpCapability, InternalHttpCapabilityIssuerFn};

/// Re-exported so every channel that records guest-supplied `gen_ai.*` metadata
/// bounds it the same way, including the dispatch callback path in
/// `temper-server`. A second copy of the bound would drift. See ADR-0166.
pub use span_hints::{MAX_REDACTED_LLM_METADATA_VALUE_BYTES, clamp_redacted_metadata_value};
#[cfg(test)]
pub(crate) use span_hints::{MAX_RESPONSE_CAPTURE_BYTES, SpanHints};
pub(crate) use span_hints::{
    apply_response_captures, apply_span_hints, clamp_llm_metadata_json,
    datadog_visible_span_hint_field, is_llm_namespace_key, llm_namespace_attr_allowed,
    redact_llm_content_hints, span_hint_otel_name, split_span_hint_headers, truncate_for_span_attr,
};

/// Host capabilities provided to WASM modules.
///
/// Production uses real HTTP + secret store. Simulation uses canned
/// responses for deterministic testing.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HttpBatchRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HttpBatchResponse {
    pub status: u16,
    pub body: String,
}

#[async_trait]
pub trait WasmHost: Send + Sync {
    /// Maximum raw request bytes admitted before parsing.
    fn temper_data_request_budget(&self) -> usize {
        0
    }

    /// Maximum simultaneously open application-data responses.
    fn temper_data_response_handle_budget(&self) -> usize {
        0
    }

    /// Execute a canonical Temper application-data request.
    async fn temper_data_call(&self, _request: &[u8]) -> Result<Vec<u8>, String> {
        Err("application-data capability is not bound for this invocation".into())
    }

    /// Read a bounded chunk from an invocation-scoped File stream.
    fn temper_file_stream_read(&self, _handle: u32, _max_bytes: usize) -> Result<Vec<u8>, i32> {
        Err(-3)
    }

    /// Try to write a bounded chunk to an invocation-scoped File stream.
    fn temper_file_stream_try_write(&self, _handle: u32, _bytes: &[u8]) -> Result<usize, i32> {
        Err(-3)
    }

    /// Whether this invocation's tenant opted into exporting raw LLM content to
    /// telemetry (ADR-0166). Defaults to `false` so a host that does not answer
    /// redacts: the guest-facing telemetry APIs read this, and the only safe
    /// default for a channel fed by an untrusted module is to withhold content.
    fn exports_llm_content(&self) -> bool {
        false
    }

    /// Make an HTTP request. Returns (status_code, response_body).
    async fn http_call(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<(u16, String), String>;

    /// Make multiple HTTP requests concurrently. Returns responses in request order.
    async fn http_call_batch(
        &self,
        requests: &[HttpBatchRequest],
    ) -> Result<Vec<HttpBatchResponse>, String> {
        let results = futures_util::future::join_all(requests.iter().map(|request| async {
            self.http_call(
                &request.method,
                &request.url,
                request.headers.as_slice(),
                &request.body,
            )
            .await
            .map(|(status, body)| HttpBatchResponse { status, body })
        }))
        .await;
        results.into_iter().collect()
    }

    /// Retrieve a secret by key.
    fn get_secret(&self, key: &str) -> Result<String, String>;

    /// Make an HTTP request with binary body. Returns (status_code, response_bytes).
    ///
    /// Used by streaming host functions where the request body and response are
    /// raw bytes (not UTF-8 strings). The host reads/writes bytes from/to
    /// StreamRegistry; WASM never touches raw binary data.
    async fn http_call_binary(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<(u16, Vec<u8>), String>;

    /// Make a Connect protocol server-streaming RPC call.
    ///
    /// Sends an HTTP POST with JSON body to the given URL using the Connect
    /// protocol (HTTP/1.1, `Connect-Protocol-Version: 1`). Reads the full
    /// response, parses Connect binary frames (5-byte prefix per message:
    /// 1 flag byte + 4 big-endian length bytes), and returns each data-frame
    /// payload as a JSON string.
    ///
    /// Returns a vec of decoded JSON message payloads.
    async fn connect_call(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<Vec<String>, String> {
        let _ = (url, headers, body);
        Err("connect_call not supported by this host".to_string())
    }

    /// Make a streaming HTTP request. Returns (status_code, accumulated_body).
    ///
    /// Same as `http_call` but reads the response in chunks instead of buffering
    /// the entire body at once. This is critical for SSE/streaming APIs (OpenAI,
    /// Anthropic) where the server keeps the connection open while generating.
    ///
    /// Benefits over `http_call`:
    /// - Emits progress on each chunk (keeps heartbeats alive)
    /// - Per-chunk timeout instead of total-response timeout
    /// - No memory spike from buffering large streaming responses
    ///
    /// Default implementation falls back to `http_call`.
    async fn http_call_streaming(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<(u16, String), String> {
        self.http_call(method, url, headers, body).await
    }

    // --- ADR-0057 streaming primitive (outbound, Phase 1) ---
    //
    // Opens a bidirectional streaming exchange for a single HTTP
    // call. Guests write request body chunks into
    // `handles.request_body` (closing it to signal end of request),
    // retrieve response head once available via
    // `http_stream_response_head`, then read response body chunks
    // from `handles.response_body` (empty chunk = EOF).
    //
    // Default impl: "not supported" — hosts opt in by overriding.

    /// Open an outbound streaming HTTP exchange. Returns handles
    /// the guest uses to push request body + pull response body.
    /// The host begins sending the request as soon as the first
    /// chunk is written (or immediately if body is empty and the
    /// guest closes `handles.request_body`).
    async fn http_stream_begin_outbound(
        &self,
        _request: crate::http_stream::HttpRequestHead,
    ) -> Result<crate::http_stream::HttpStreamHandles, String> {
        Err("http_stream_begin_outbound not supported by this host".to_string())
    }

    /// Read the next chunk from a stream handle. Returns empty
    /// vector on clean EOF. Blocks if no chunk is available and
    /// the peer has not closed.
    async fn http_stream_read(
        &self,
        _handle: crate::http_stream::StreamHandle,
    ) -> Result<Vec<u8>, crate::http_stream::StreamError> {
        Err(crate::http_stream::StreamError::Aborted(
            "http_stream_read not supported by this host".into(),
        ))
    }

    /// Read at most `max_bytes` from a stream handle, retaining any
    /// unread suffix for the next read. Hosts that can receive chunks
    /// larger than a guest buffer should override this.
    async fn http_stream_read_bounded(
        &self,
        handle: crate::http_stream::StreamHandle,
        max_bytes: usize,
    ) -> Result<Vec<u8>, crate::http_stream::StreamError> {
        let chunk = self.http_stream_read(handle).await?;
        if chunk.len() > max_bytes {
            return Err(crate::http_stream::StreamError::Aborted(
                "stream chunk exceeded guest buffer capacity".into(),
            ));
        }
        Ok(chunk)
    }

    /// Non-blocking write to a stream handle. Returns WouldBlock
    /// if the channel is full.
    async fn http_stream_try_write(
        &self,
        _handle: crate::http_stream::StreamHandle,
        _chunk: Vec<u8>,
    ) -> Result<usize, crate::http_stream::StreamError> {
        Err(crate::http_stream::StreamError::Aborted(
            "http_stream_try_write not supported by this host".into(),
        ))
    }

    /// Close a stream handle. Release of a sender signals EOF to
    /// the receiver; release of a receiver turns subsequent sender
    /// writes into Closed errors.
    async fn http_stream_close(
        &self,
        _handle: crate::http_stream::StreamHandle,
    ) -> Result<(), crate::http_stream::StreamError> {
        Err(crate::http_stream::StreamError::Aborted(
            "http_stream_close not supported by this host".into(),
        ))
    }

    /// Block until the response head (status + headers) is
    /// available for the given response-body handle, then return
    /// it. Guests typically call this once, after closing the
    /// request body, and before reading the response body.
    async fn http_stream_response_head(
        &self,
        _response_body: crate::http_stream::StreamHandle,
    ) -> Result<crate::http_stream::HttpResponseHead, String> {
        Err("http_stream_response_head not supported by this host".to_string())
    }

    /// Submit the response head for an inbound exchange (ADR-0069
    /// dispatch). The guest calls this once — keyed on the
    /// response-body handle it was given — before writing the
    /// response body. The kernel dispatcher blocks on
    /// `await_inbound_response_head` until this fires.
    async fn http_stream_send_response_head(
        &self,
        _response_body: crate::http_stream::StreamHandle,
        _head: crate::http_stream::HttpResponseHead,
    ) -> Result<(), crate::http_stream::StreamError> {
        Err(crate::http_stream::StreamError::Aborted(
            "http_stream_send_response_head not supported by this host".into(),
        ))
    }

    /// Log a message at the given level.
    fn log(&self, level: &str, message: &str);

    /// Evaluate a single transition against an IOA spec.
    ///
    /// Generic platform capability: any WASM module can validate transitions.
    /// The host builds a TransitionTable from the IOA source and evaluates
    /// the given action from the given state with the given parameters.
    ///
    /// Returns a JSON result: `{ "success": bool, "new_state": str, "error": str|null, "guard_result": str|null }`
    ///
    /// Default: not supported (overridden in temper-server where temper-jit is available).
    fn evaluate_spec(
        &self,
        _ioa_source: &str,
        _current_state: &str,
        _action: &str,
        _params_json: &str,
    ) -> Result<String, String> {
        Err("evaluate_spec not supported by this host".to_string())
    }

    /// Emit a replayable progress event from the guest module.
    fn emit_progress(&self, _event_json: &str) -> Result<(), String> {
        Ok(())
    }

    /// Emit a Temper wide event from the guest module.
    fn emit_wide_event(&self, _event_json: &str) -> Result<(), String> {
        Ok(())
    }

    /// Emit a structured log event from the guest module.
    fn log_structured(&self, _log_json: &str) -> Result<(), String> {
        Ok(())
    }

    /// Emit a metric directly from the guest module.
    fn emit_metric(&self, _metric_json: &str) -> Result<(), String> {
        Ok(())
    }
}

/// Callback for evaluating IOA spec transitions.
///
/// Injected by `temper-server` where `temper-jit` is available.
/// Keeps the dependency boundary clean: `temper-wasm` never depends on `temper-jit`.
pub type SpecEvaluatorFn =
    Arc<dyn Fn(&str, &str, &str, &str) -> Result<String, String> + Send + Sync>;

/// Callback for replayable progress events emitted by guest WASM modules.
pub type ProgressEmitterFn = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// Callback for lazily resolving a secret by key.
///
/// Production hosts can use this to avoid eagerly materializing every tenant
/// secret before invocation. When configured, the resolver is authoritative for
/// guest `host_get_secret` lookups; the eager secret map remains available for
/// host-internal bootstrap behavior. The callback is responsible for any
/// authorization checks needed before returning the plaintext value.
pub type SecretResolverFn = Arc<dyn Fn(&str) -> Result<String, String> + Send + Sync>;

/// Future returned by a binary HTTP interceptor.
pub type BinaryHttpInterceptorFuture =
    Pin<Box<dyn Future<Output = Option<Result<(u16, Vec<u8>), String>>> + Send>>;

/// Optional callback that can short-circuit binary HTTP requests.
///
/// This lets the server handle specific local transport paths directly
/// (for example, internal blob storage) without going back through loopback HTTP.
pub type BinaryHttpInterceptorFn = Arc<
    dyn Fn(String, String, Vec<(String, String)>, Vec<u8>) -> BinaryHttpInterceptorFuture
        + Send
        + Sync,
>;

/// Optional callback that can short-circuit text HTTP requests.
///
/// This lets the server handle specific local transport paths directly
/// (for example, internal OData `$value` reads/writes) without going back
/// through loopback HTTP when the guest expects a UTF-8 body.
pub type TextHttpInterceptorFn = Arc<
    dyn Fn(String, String, Vec<(String, String)>, String) -> TextHttpInterceptorFuture
        + Send
        + Sync,
>;

type TextHttpInterceptorFuture =
    Pin<Box<dyn Future<Output = Option<Result<(u16, String), String>>> + Send>>;

/// Host-only application-data service callback bound to one invocation authority.
pub type TemperDataCallFn = Arc<
    dyn Fn(Vec<u8>) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + Send>> + Send + Sync,
>;

/// Invocation-scoped File stream read callback.
pub type TemperFileReadFn = Arc<dyn Fn(u32, usize) -> Result<Vec<u8>, i32> + Send + Sync>;

/// Invocation-scoped File stream write callback.
pub type TemperFileWriteFn = Arc<dyn Fn(u32, Vec<u8>) -> Result<usize, i32> + Send + Sync>;

/// Production host: real HTTP calls via reqwest, real secrets.
pub struct ProductionWasmHost {
    /// HTTP client for making real requests.
    client: reqwest::Client,
    /// Separate client whose redirect policy is disabled for capabilities.
    internal_client: reqwest::Client,
    /// Secrets from env vars or a secret store.
    secrets: BTreeMap<String, String>,
    /// Optional lazy secret resolver authoritative for guest secret lookups.
    secret_resolver: Option<SecretResolverFn>,
    /// Canonical base URL for the local Temper API when known directly by the server.
    internal_api_base_url: Option<String>,
    /// Trusted callback that issues one request-bound internal capability.
    internal_capability_issuer: Option<InternalHttpCapabilityIssuerFn>,
    /// Optional spec evaluator (provided by temper-server at construction).
    spec_evaluator: Option<SpecEvaluatorFn>,
    /// Optional progress emitter (provided by temper-server at construction).
    progress_emitter: Option<ProgressEmitterFn>,
    /// W3C trace ID for auto-injecting traceparent headers in HTTP calls.
    trace_id: Option<String>,
    /// Optional short-circuit for binary HTTP calls.
    binary_http_interceptor: Option<BinaryHttpInterceptorFn>,
    /// Optional short-circuit for text HTTP calls.
    text_http_interceptor: Option<TextHttpInterceptorFn>,
    /// Governed application-data service; absent means fail closed.
    data_call: Option<TemperDataCallFn>,
    /// Invocation-scoped File stream callbacks.
    file_read: Option<TemperFileReadFn>,
    file_write: Option<TemperFileWriteFn>,
    data_request_budget: usize,
    data_response_handle_budget: usize,
    /// Invocation context for auto-enriching guest telemetry.
    invocation_context: Option<WasmInvocationContext>,
    /// Whether this invocation's tenant may export raw LLM content (prompts,
    /// completions, system instructions, tool arguments/results) captured from
    /// guest HTTP calls to the telemetry backend. Defaults to `false` (redact);
    /// the server sets it per tenant. See ADR-0166.
    export_llm_content: bool,
    /// Registry of active streaming HTTP exchanges (ADR-0057).
    /// One per host instance; handle IDs are unique within the host.
    http_streams: Arc<crate::http_stream::HttpStreamRegistry>,
}

#[derive(Debug, Deserialize)]
struct GuestWideEventInput {
    kind: EventKind,
    #[serde(default)]
    operation: Option<String>,
    #[serde(default)]
    success: Option<bool>,
    #[serde(default)]
    duration_ns: Option<u64>,
    #[serde(default)]
    from_status: Option<String>,
    #[serde(default)]
    to_status: Option<String>,
    #[serde(default)]
    tags: BTreeMap<String, String>,
    #[serde(default)]
    attributes: BTreeMap<String, Value>,
    #[serde(default)]
    measurements: BTreeMap<String, f64>,
}

#[derive(Debug, Deserialize)]
struct GuestStructuredLogInput {
    level: String,
    message: String,
    #[serde(default)]
    fields: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize)]
struct GuestMetricInput {
    name: String,
    value: f64,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    tags: BTreeMap<String, String>,
}

fn guest_metric_is_counter_kind(kind: Option<&str>) -> bool {
    matches!(kind, Some("count" | "counter"))
}

fn guest_metric_tag_allowed(key: &str) -> bool {
    let key = key.trim();
    if key.is_empty() {
        return false;
    }
    !matches!(
        key,
        "trace_id"
            | "span_id"
            | "dd.trace_id"
            | "dd.span_id"
            | "otel.trace_id"
            | "otel.span_id"
            | "session_id"
            | "entity_id"
            | "workflow_run_id"
            | "tool.call_id"
            | "request.id"
    ) && !key.ends_with("_id")
}

const DEFAULT_BLOB_TRANSPORT_MAX_CONCURRENCY: usize = 32;

fn blob_transport_max_concurrency() -> usize {
    static MAX_CONCURRENCY: OnceLock<usize> = OnceLock::new();
    *MAX_CONCURRENCY.get_or_init(|| {
        std::env::var("TEMPER_BLOB_TRANSPORT_MAX_CONCURRENCY")
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .filter(|value| *value > 0)
            .or_else(|| {
                std::env::var("TEMPER_BLOB_IO_MAX_CONCURRENCY")
                    .ok()
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .filter(|value| *value > 0)
            })
            .unwrap_or(DEFAULT_BLOB_TRANSPORT_MAX_CONCURRENCY)
    })
}

fn blob_transport_semaphore() -> &'static Semaphore {
    static SEMAPHORE: OnceLock<Semaphore> = OnceLock::new();
    SEMAPHORE.get_or_init(|| Semaphore::new(blob_transport_max_concurrency()))
}

fn build_production_http_client(
    secrets: &BTreeMap<String, String>,
    timeout: std::time::Duration,
    disable_redirects: bool,
) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(timeout);
    if disable_redirects {
        builder = builder
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy();
    }

    for (key, pem) in secrets {
        if !key.starts_with("ca_cert:") {
            continue;
        }
        match reqwest::Certificate::from_pem(pem.as_bytes()) {
            Ok(cert) => {
                builder = builder.add_root_certificate(cert);
            }
            Err(error) => {
                tracing::warn!(key, error = %error, "failed to parse CA certificate from secret");
            }
        }
    }

    builder
        .build()
        .expect("production HTTP client configuration must be valid")
}

fn remote_blob_backend<'a>(secrets: &'a BTreeMap<String, String>, url: &str) -> Option<&'a str> {
    let endpoint = secrets.get("blob_endpoint")?.trim_end_matches('/');
    if endpoint.is_empty() || !url.starts_with(endpoint) {
        return None;
    }

    if endpoint.contains("/_internal/blobs") {
        return None;
    }

    let backend = if endpoint.contains("amazonaws.com") || endpoint.contains(".s3.") {
        "s3"
    } else if endpoint.contains("r2.cloudflarestorage.com") {
        "r2"
    } else {
        "custom"
    };
    Some(backend)
}

/// Apply ADR-0166 to a map of guest-supplied string tags. Shared by guest wide
/// events and guest metrics: both let an untrusted module choose tag names *and*
/// string values, so both are the same channel as the span APIs.
fn redact_guest_string_tags(tags: &mut BTreeMap<String, String>, export_llm_content: bool) {
    if export_llm_content {
        return;
    }
    tags.retain(|key, _| llm_namespace_attr_allowed(key));
    for (key, value) in tags.iter_mut() {
        if is_llm_namespace_key(key)
            && let Some(clamped) = clamp_redacted_metadata_value(value)
        {
            *value = clamped;
        }
    }
}

/// Apply ADR-0166 to the guest-supplied half of a wide event. Inside the
/// `gen_ai.*` namespace a non-opted-in tenant keeps only recognised, bounded
/// metadata; keys outside it are the module's own application telemetry and are
/// left alone, matching the guest-span boundary.
fn redact_guest_wide_event_fields(
    tags: &mut BTreeMap<String, String>,
    attributes: &mut BTreeMap<String, Value>,
    export_llm_content: bool,
) {
    if export_llm_content {
        return;
    }
    redact_guest_string_tags(tags, export_llm_content);
    attributes.retain(|key, value| {
        if !llm_namespace_attr_allowed(key) {
            return false;
        }
        // A structured value under a recognised metadata key is dropped, not
        // clamped: see `clamp_llm_metadata_json`.
        !is_llm_namespace_key(key) || clamp_llm_metadata_json(value).is_some()
    });
    for (key, value) in attributes.iter_mut() {
        if is_llm_namespace_key(key)
            && let Some(clamped) = clamp_llm_metadata_json(value)
        {
            *value = clamped;
        }
    }
}

impl ProductionWasmHost {
    /// Create with pre-loaded secrets and default HTTP timeout.
    ///
    /// The default timeout matches `WasmResourceLimits::default().max_duration`
    /// (120s per ADR-0045).
    pub fn new(secrets: BTreeMap<String, String>) -> Self {
        Self::with_timeout(secrets, crate::WasmResourceLimits::default().max_duration)
    }

    /// Create with a pre-existing [`HttpStreamRegistry`] so the
    /// ADR-0069 Phase 2 dispatcher and the per-request host share
    /// the same registry — handles minted by the dispatcher are
    /// reachable via FFI from the guest.
    pub fn with_shared_streams(
        secrets: BTreeMap<String, String>,
        http_streams: Arc<crate::http_stream::HttpStreamRegistry>,
    ) -> Self {
        let mut host = Self::new(secrets);
        host.http_streams = http_streams;
        host
    }

    /// Borrow the host's shared stream registry. Used by the HTTP
    /// dispatcher to mint inbound exchanges.
    pub fn http_streams(&self) -> Arc<crate::http_stream::HttpStreamRegistry> {
        self.http_streams.clone()
    }

    /// Create with pre-loaded secrets and a custom HTTP request timeout.
    ///
    /// Secrets whose key starts with `ca_cert:` are treated as PEM-encoded
    /// CA certificates and added as trusted roots to the HTTP client. This
    /// lets operators provision private CA trust via the same secret store
    /// that WASM modules already use, with no filesystem or env var coupling.
    pub fn with_timeout(secrets: BTreeMap<String, String>, timeout: std::time::Duration) -> Self {
        let client = build_production_http_client(&secrets, timeout, false);
        let internal_client = build_production_http_client(&secrets, timeout, true);

        Self {
            client,
            internal_client,
            secrets,
            secret_resolver: None,
            internal_api_base_url: None,
            internal_capability_issuer: None,
            spec_evaluator: None,
            progress_emitter: None,
            trace_id: None,
            binary_http_interceptor: None,
            text_http_interceptor: None,
            data_call: None,
            file_read: None,
            file_write: None,
            data_request_budget: 0,
            data_response_handle_budget: 0,
            invocation_context: None,
            export_llm_content: false,
            http_streams: Arc::new(crate::http_stream::HttpStreamRegistry::new()),
        }
    }

    /// Create with a spec evaluator for `host_evaluate_spec` support.
    pub fn with_spec_evaluator(mut self, evaluator: SpecEvaluatorFn) -> Self {
        self.spec_evaluator = Some(evaluator);
        self
    }

    /// Create with a progress emitter for `host_emit_progress` support.
    pub fn with_progress_emitter(mut self, emitter: ProgressEmitterFn) -> Self {
        self.progress_emitter = Some(emitter);
        self
    }

    /// Create with a lazy secret resolver for `host_get_secret` support.
    pub fn with_secret_resolver(mut self, resolver: SecretResolverFn) -> Self {
        self.secret_resolver = Some(resolver);
        self
    }

    /// Set the W3C trace ID for auto-injecting `traceparent` in HTTP calls.
    pub fn with_trace_id(mut self, trace_id: Option<String>) -> Self {
        self.trace_id = trace_id.filter(|s| !s.is_empty());
        self
    }

    /// Attach invocation context for guest telemetry auto-enrichment.
    pub fn with_invocation_context(mut self, context: WasmInvocationContext) -> Self {
        self.invocation_context = Some(context);
        self
    }

    /// Set whether this invocation's tenant may export raw LLM content captured
    /// from guest HTTP calls. Defaults to `false` (redact). See ADR-0166.
    pub fn with_llm_content_export(mut self, export_content: bool) -> Self {
        self.export_llm_content = export_content;
        self
    }

    /// Attach the canonical local Temper API base URL for internal call detection.
    pub fn with_internal_api_base_url(mut self, api_url: Option<String>) -> Self {
        self.internal_api_base_url = api_url
            .map(|value| value.trim_end_matches('/').to_string())
            .filter(|value| !value.is_empty());
        self
    }

    /// Attach the trusted issuer for request-bound internal capabilities.
    pub fn with_internal_capability_issuer(
        mut self,
        issuer: InternalHttpCapabilityIssuerFn,
    ) -> Self {
        self.internal_capability_issuer = Some(issuer);
        self
    }

    /// Create with a binary HTTP interceptor for local fast paths.
    pub fn with_binary_http_interceptor(mut self, interceptor: BinaryHttpInterceptorFn) -> Self {
        self.binary_http_interceptor = Some(interceptor);
        self
    }

    /// Create with a text HTTP interceptor for local fast paths.
    pub fn with_text_http_interceptor(mut self, interceptor: TextHttpInterceptorFn) -> Self {
        self.text_http_interceptor = Some(interceptor);
        self
    }

    /// Bind the governed application-data and File stream services.
    pub fn with_temper_data_service(
        mut self,
        data_call: TemperDataCallFn,
        file_read: TemperFileReadFn,
        file_write: TemperFileWriteFn,
        budgets: &temper_wasm_sdk::data::ModuleDataBudgets,
    ) -> Self {
        self.data_call = Some(data_call);
        self.file_read = Some(file_read);
        self.file_write = Some(file_write);
        self.data_request_budget = budgets.max_request_bytes as usize;
        self.data_response_handle_budget = budgets.max_open_responses as usize;
        self
    }

    fn is_internal_temper_url(&self, url: &str) -> bool {
        // Only the server-owned configuration can classify a target as
        // internal. A tenant secret or guest integration value must never
        // redirect a request-bound capability to an attacker-controlled host.
        let configured = self.internal_api_base_url.iter().map(String::as_str);
        internal_http::is_internal_url(url, configured)
    }

    fn add_internal_temper_headers(
        &self,
        mut builder: reqwest::RequestBuilder,
        method: &str,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<reqwest::RequestBuilder, String> {
        if !self.is_internal_temper_url(url) {
            return Ok(builder);
        }

        let issuer = self.internal_capability_issuer.as_ref().ok_or_else(|| {
            "internal Temper HTTP call has no authenticated capability issuer".to_string()
        })?;
        let canonical_method = method.to_ascii_uppercase();
        let capability = issuer(&canonical_method, url)?;
        builder = builder
            .header(
                "authorization",
                format!("Bearer {}", capability.bearer_token()),
            )
            .header("x-tenant-id", capability.tenant());

        if let Some(invocation_context) = &self.invocation_context {
            builder = add_workflow_observability_headers(builder, headers, invocation_context);
        }
        Ok(builder)
    }

    fn outbound_headers(
        &self,
        url: &str,
        headers: &[(String, String)],
    ) -> (bool, Vec<(String, String)>) {
        let is_internal = self.is_internal_temper_url(url);
        let headers = if is_internal {
            internal_http::sanitize_internal_headers(headers)
        } else {
            headers.to_vec()
        };
        (is_internal, headers)
    }

    fn build_guest_wide_event(&self, event_json: &str) -> Result<WideEvent, String> {
        let payload: GuestWideEventInput = serde_json::from_str(event_json)
            .map_err(|e| format!("invalid guest wide event payload: {e}"))?;

        let mut tags = payload.tags;
        let mut attributes = payload.attributes;
        let measurements = payload.measurements;

        // ADR-0166: a wide event is a guest-authored telemetry record with
        // guest-chosen tag and attribute names, so it is the same untrusted
        // channel as the span APIs and takes the same per-tenant decision. Applied
        // before any host-derived field is merged in below, so only guest input is
        // judged.
        redact_guest_wide_event_fields(&mut tags, &mut attributes, self.export_llm_content);

        let entity_type = self
            .invocation_context
            .as_ref()
            .map(|ctx| ctx.entity_type.clone())
            .or_else(|| tags.get("entity_type").cloned())
            .unwrap_or_else(|| "WasmGuest".to_string());
        let entity_id = self
            .invocation_context
            .as_ref()
            .map(|ctx| ctx.entity_id.clone())
            .or_else(|| {
                attributes
                    .get("entity_id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .unwrap_or_default();
        let trace_id = self
            .invocation_context
            .as_ref()
            .map(|ctx| ctx.trace_id.clone())
            .unwrap_or_default();
        let operation = payload
            .operation
            .or_else(|| infer_guest_operation(payload.kind, &tags))
            .unwrap_or_else(|| "guest_event".to_string());
        let success = payload
            .success
            .or_else(|| tags.get("success").map(|value| value == "true"))
            .unwrap_or(true);
        let duration_ns = payload.duration_ns.unwrap_or_else(|| {
            measurements
                .get("duration_ms")
                .map(|value| (value * 1_000_000.0) as u64)
                .unwrap_or_default()
        });

        tags.entry("entity_type".into())
            .or_insert_with(|| entity_type.clone());
        if let Some(ctx) = &self.invocation_context {
            attributes
                .entry("tenant".into())
                .or_insert_with(|| json!(ctx.tenant));
            attributes
                .entry("trigger_action".into())
                .or_insert_with(|| json!(ctx.trigger_action));
            if let Some(agent_id) = &ctx.agent_id {
                attributes
                    .entry("agent_id".into())
                    .or_insert_with(|| json!(agent_id));
            }
            if let Some(module) = &ctx.wasm_module {
                attributes
                    .entry("wasm_module".into())
                    .or_insert_with(|| json!(module));
            }
            if let Some(session_id) = &ctx.session_id {
                attributes
                    .entry("gen_ai.conversation.id".into())
                    .or_insert_with(|| json!(session_id));
            }
            if let Some(run_id) = &ctx.workflow_run_id {
                attributes
                    .entry("workflow_run_id".into())
                    .or_insert_with(|| json!(run_id));
            }
        }
        attributes
            .entry("entity_id".into())
            .or_insert_with(|| json!(entity_id));

        Ok(WideEvent {
            event_kind: payload.kind,
            entity_type,
            entity_id,
            operation,
            from_status: payload.from_status.unwrap_or_default(),
            to_status: payload.to_status.unwrap_or_default(),
            success,
            duration_ns,
            timestamp: chrono::Utc::now(),
            trace_id,
            span_id: Uuid::new_v4().to_string(),
            tags,
            attributes,
            measurements,
        })
    }
}

#[async_trait]
impl WasmHost for ProductionWasmHost {
    fn temper_data_request_budget(&self) -> usize {
        self.data_request_budget
    }

    fn temper_data_response_handle_budget(&self) -> usize {
        self.data_response_handle_budget
    }
    async fn temper_data_call(&self, request: &[u8]) -> Result<Vec<u8>, String> {
        let callback = self.data_call.as_ref().ok_or_else(|| {
            "application-data capability is not bound for this invocation".to_string()
        })?;
        callback(request.to_vec()).await
    }

    fn temper_file_stream_read(&self, handle: u32, max_bytes: usize) -> Result<Vec<u8>, i32> {
        self.file_read.as_ref().ok_or(-3)?(handle, max_bytes)
    }

    fn temper_file_stream_try_write(&self, handle: u32, bytes: &[u8]) -> Result<usize, i32> {
        self.file_write.as_ref().ok_or(-3)?(handle, bytes.to_vec())
    }

    fn exports_llm_content(&self) -> bool {
        self.export_llm_content
    }

    async fn http_call(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<(u16, String), String> {
        let started = Instant::now();
        // Strip Temper span hint headers (X-Temper-Span-*) before the request
        // is built, and capture them for the local tracing span. See
        // ADR-0037: WASM guests annotate outgoing calls with
        // `X-Temper-Span-Name` / `X-Temper-Span-Attr-*` so the resulting
        // span has a semantically meaningful name (e.g., `tool.llm_call`)
        // and attributes (e.g., `gen_ai.request.model`).
        let (filtered_headers, mut span_hints) = split_span_hint_headers(headers);
        // ARN-243: drop LLM content from span hints unless this tenant opted in.
        redact_llm_content_hints(&mut span_hints, self.export_llm_content);
        let (is_internal, outbound_headers) = self.outbound_headers(url, &filtered_headers);
        if let Some(interceptor) = &self.text_http_interceptor
            && let Some(result) = interceptor(
                method.to_string(),
                url.to_string(),
                outbound_headers.clone(),
                body.to_string(),
            )
            .await
        {
            return result;
        }
        let span = tracing::info_span!(
            "wasm.host.http_call",
            otel.name = %span_hint_otel_name(&span_hints, "wasm.host.http_call"),
            http.method = %method,
            http.url = %telemetry_url(url),
            request_bytes = body.len() as u64,
            header_count = outbound_headers.len() as u64,
            status_code = tracing::field::Empty,
            response_bytes = tracing::field::Empty,
            duration_ms = tracing::field::Empty,
            tenant = tracing::field::Empty,
            wasm_module = tracing::field::Empty,
            observability_event = tracing::field::Empty,
            session_id = tracing::field::Empty,
            managed_session_id = tracing::field::Empty,
            inner_session_id = tracing::field::Empty,
            parent_session_id = tracing::field::Empty,
            agent_id = tracing::field::Empty,
            environment_id = tracing::field::Empty,
            entity_type = tracing::field::Empty,
            entity_id = tracing::field::Empty,
            trigger_action = tracing::field::Empty,
            action_name = tracing::field::Empty,
            workflow_step = tracing::field::Empty,
            workflow_root_entity_type = tracing::field::Empty,
            workflow_root_entity_id = tracing::field::Empty,
            workflow_run_id = tracing::field::Empty,
            dd_llmobs_enabled = tracing::field::Empty,
            tool.name = tracing::field::Empty,
            tool.call_id = tracing::field::Empty,
            gen_ai.operation.name = tracing::field::Empty,
            gen_ai.provider.name = tracing::field::Empty,
            gen_ai.system = tracing::field::Empty,
            gen_ai.request.model = tracing::field::Empty,
            gen_ai.response.model = tracing::field::Empty,
            gen_ai.request.temperature = tracing::field::Empty,
            gen_ai.request.max_tokens = tracing::field::Empty,
            gen_ai.conversation.id = tracing::field::Empty,
            gen_ai.system_instructions = tracing::field::Empty,
            gen_ai.input.messages = tracing::field::Empty,
            gen_ai.prompt = tracing::field::Empty,
            gen_ai.output.messages = tracing::field::Empty,
            gen_ai.completion = tracing::field::Empty,
            gen_ai.response.finish_reasons = tracing::field::Empty,
            gen_ai.usage.input_tokens = tracing::field::Empty,
            gen_ai.usage.output_tokens = tracing::field::Empty,
            gen_ai.usage.cache_read_input_tokens = tracing::field::Empty,
            gen_ai.usage.cache_creation_input_tokens = tracing::field::Empty,
            http.response.body.size = tracing::field::Empty,
        );
        record_invocation_context_on_span(&span, self.invocation_context.as_ref(), "http_call");
        apply_span_hints(&span, &span_hints);
        let _guard = span.enter();

        let client = if is_internal {
            &self.internal_client
        } else {
            &self.client
        };
        let mut builder = match method.to_uppercase().as_str() {
            "GET" => client.get(url),
            "POST" => client.post(url),
            "PUT" => client.put(url),
            "DELETE" => client.delete(url),
            "PATCH" => client.patch(url),
            other => return Err(format!("unsupported HTTP method: {other}")),
        };

        for (k, v) in &outbound_headers {
            builder = builder.header(k.as_str(), v.as_str());
        }

        builder = self.add_internal_temper_headers(builder, method, url, &outbound_headers)?;
        // determinism-ok: is_internal check uses non-deterministic URL comparison,
        // but this runs in WasmHost (not simulation), so wall-clock/network access is fine.

        // Auto-inject traceparent for cross-request trace correlation.
        if !outbound_headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("traceparent"))
            && let Some(traceparent) =
                current_traceparent_header(&tracing::Span::current(), self.trace_id.as_deref())
        {
            builder = builder.header("traceparent", traceparent);
        }

        if !body.is_empty() {
            builder = builder.body(body.to_string());
        }

        let resp = builder.send().await.map_err(|e| {
            tracing::warn!(
                error = %e,
                duration_ms = started.elapsed().as_millis() as u64,
                "WASM host HTTP request failed"
            );
            format!("HTTP request failed: {e}")
        })?;
        let status = resp.status().as_u16();

        // Loud auth failure logging for internal API calls (ADR-0043).
        if is_internal && (status == 401 || status == 403) {
            let (module, agent) = self
                .invocation_context
                .as_ref()
                .map(|c| (c.entity_type.as_str(), c.agent_id.as_deref().unwrap_or("?")))
                .unwrap_or(("?", "?"));
            tracing::warn!(
                status = status,
                url = %telemetry_url(url),
                module = module,
                agent_id = agent,
                "WASM internal API call auth failure — check principal headers"
            );
        }

        // Auto-detect SSE streaming responses and use chunked reading.
        // This avoids the total-response timeout killing long-running LLM generations.
        // Detection: Content-Type text/event-stream OR request body contained "stream":true
        // (some endpoints like OpenAI Codex return application/json CT even for SSE).
        let ct_is_sse = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.contains("text/event-stream"));
        let request_asked_for_stream =
            body.contains("\"stream\":true") || body.contains("\"stream\": true");
        let is_sse = ct_is_sse || (request_asked_for_stream && (200..300).contains(&status));

        let resp_body = if is_sse {
            // SSE streaming: read chunks with per-chunk stall timeout, strip SSE
            // framing, return concatenated `data:` payloads separated by newlines.
            // This is format-agnostic — the WASM guest (llm_caller) handles
            // provider-specific parsing (OpenAI events, Anthropic events, etc.).
            let mut accumulated_data = String::new();
            let mut partial_line = String::new();
            let mut stream = resp.bytes_stream();
            let chunk_stall = std::time::Duration::from_secs(120);
            let mut chunk_count: u64 = 0;
            loop {
                match tokio::time::timeout(chunk_stall, futures_util::StreamExt::next(&mut stream))
                    .await
                {
                    Ok(Some(Ok(chunk))) => {
                        chunk_count += 1;
                        if let Ok(text) = std::str::from_utf8(&chunk) {
                            partial_line.push_str(text);
                            // Process complete lines, keep partial for next chunk
                            while let Some(nl) = partial_line.find('\n') {
                                let line = partial_line[..nl].trim_end_matches('\r');
                                if let Some(data) = line.strip_prefix("data: ") {
                                    if !accumulated_data.is_empty() {
                                        accumulated_data.push('\n');
                                    }
                                    accumulated_data.push_str(data);
                                }
                                partial_line = partial_line[nl + 1..].to_string();
                            }
                        }
                        // Emit progress to keep heartbeats alive
                        if chunk_count.is_multiple_of(20)
                            && let Some(ref emitter) = self.progress_emitter
                        {
                            let _ = emitter(&format!(
                                "{{\"kind\":\"streaming_progress\",\"chunks\":{chunk_count},\"data_bytes\":{}}}",
                                accumulated_data.len()
                            ));
                        }
                    }
                    Ok(Some(Err(e))) => {
                        tracing::warn!(error = %e, chunks = chunk_count, "SSE chunk read error");
                        break;
                    }
                    Ok(None) => break,
                    Err(_) => {
                        return Err(format!(
                            "SSE streaming stall: no data for {}s after {chunk_count} chunks",
                            chunk_stall.as_secs(),
                        ));
                    }
                }
            }
            // Process any remaining partial line
            let remaining = partial_line.trim();
            if let Some(data) = remaining.strip_prefix("data: ") {
                if !accumulated_data.is_empty() {
                    accumulated_data.push('\n');
                }
                accumulated_data.push_str(data);
            }
            accumulated_data
        } else {
            resp.text().await.map_err(|e| {
                tracing::warn!(
                    status_code = status,
                    error = %e,
                    duration_ms = started.elapsed().as_millis() as u64,
                    "WASM host HTTP response read failed"
                );
                format!("failed to read response body: {e}")
            })?
        };
        let duration_ms = started.elapsed().as_millis() as u64;
        tracing::Span::current().record("status_code", status);
        tracing::Span::current().record("response_bytes", resp_body.len() as u64);
        tracing::Span::current().record("duration_ms", duration_ms);
        apply_response_captures(
            &tracing::Span::current(),
            &resp_body,
            &span_hints.response_captures,
        );
        metrics::record_host_http_call(
            method,
            "text",
            status,
            body.len() as u64,
            resp_body.len() as u64,
            duration_ms as f64,
        );
        Ok((status, resp_body))
    }

    async fn http_call_binary(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<(u16, Vec<u8>), String> {
        let started = Instant::now();
        // See http_call for the span-hint-header rationale (ADR-0037).
        let (filtered_headers, mut span_hints) = split_span_hint_headers(headers);
        // ARN-243: drop LLM content from span hints unless this tenant opted in.
        redact_llm_content_hints(&mut span_hints, self.export_llm_content);
        let (is_internal, outbound_headers) = self.outbound_headers(url, &filtered_headers);
        let span = tracing::info_span!(
            "wasm.host.http_call_binary",
            otel.name = %span_hint_otel_name(&span_hints, "wasm.host.http_call_binary"),
            http.method = %method,
            http.url = %telemetry_url(url),
            request_bytes = body.len() as u64,
            header_count = outbound_headers.len() as u64,
            status_code = tracing::field::Empty,
            response_bytes = tracing::field::Empty,
            duration_ms = tracing::field::Empty,
            tenant = tracing::field::Empty,
            wasm_module = tracing::field::Empty,
            observability_event = tracing::field::Empty,
            session_id = tracing::field::Empty,
            managed_session_id = tracing::field::Empty,
            inner_session_id = tracing::field::Empty,
            parent_session_id = tracing::field::Empty,
            agent_id = tracing::field::Empty,
            environment_id = tracing::field::Empty,
            entity_type = tracing::field::Empty,
            entity_id = tracing::field::Empty,
            trigger_action = tracing::field::Empty,
            action_name = tracing::field::Empty,
            workflow_step = tracing::field::Empty,
            workflow_root_entity_type = tracing::field::Empty,
            workflow_root_entity_id = tracing::field::Empty,
            workflow_run_id = tracing::field::Empty,
            dd_llmobs_enabled = tracing::field::Empty,
            tool.name = tracing::field::Empty,
            tool.call_id = tracing::field::Empty,
            gen_ai.operation.name = tracing::field::Empty,
            gen_ai.provider.name = tracing::field::Empty,
            gen_ai.system = tracing::field::Empty,
            gen_ai.request.model = tracing::field::Empty,
            gen_ai.response.model = tracing::field::Empty,
            gen_ai.request.temperature = tracing::field::Empty,
            gen_ai.request.max_tokens = tracing::field::Empty,
            gen_ai.conversation.id = tracing::field::Empty,
            gen_ai.system_instructions = tracing::field::Empty,
            gen_ai.input.messages = tracing::field::Empty,
            gen_ai.prompt = tracing::field::Empty,
            gen_ai.output.messages = tracing::field::Empty,
            gen_ai.completion = tracing::field::Empty,
            gen_ai.response.finish_reasons = tracing::field::Empty,
            gen_ai.usage.input_tokens = tracing::field::Empty,
            gen_ai.usage.output_tokens = tracing::field::Empty,
            gen_ai.usage.cache_read_input_tokens = tracing::field::Empty,
            gen_ai.usage.cache_creation_input_tokens = tracing::field::Empty,
            http.response.body.size = tracing::field::Empty,
        );
        record_invocation_context_on_span(
            &span,
            self.invocation_context.as_ref(),
            "http_call_binary",
        );
        apply_span_hints(&span, &span_hints);
        let _guard = span.enter();

        if let Some(ref interceptor) = self.binary_http_interceptor
            && let Some(result) = interceptor(
                method.to_string(),
                url.to_string(),
                outbound_headers.clone(),
                body.to_vec(),
            )
            .await
        {
            let duration_ms = started.elapsed().as_millis() as u64;
            match result {
                Ok((status, resp_bytes)) => {
                    tracing::Span::current().record("status_code", status);
                    tracing::Span::current().record("response_bytes", resp_bytes.len() as u64);
                    tracing::Span::current().record("duration_ms", duration_ms);
                    metrics::record_host_http_call(
                        method,
                        "binary",
                        status,
                        body.len() as u64,
                        resp_bytes.len() as u64,
                        duration_ms as f64,
                    );
                    return Ok((status, resp_bytes));
                }
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        duration_ms,
                        "WASM host binary interceptor failed"
                    );
                    return Err(error);
                }
            }
        }

        let _blob_transport_permit = if let Some(backend) = remote_blob_backend(&self.secrets, url)
        {
            let queued_at = Instant::now();
            let permit = blob_transport_semaphore()
                .acquire()
                .await
                .expect("blob transport semaphore should not be closed");
            let wait_duration = queued_at.elapsed();
            let wait_ms = wait_duration.as_millis() as u64;
            if wait_ms > 0 {
                tracing::info!(
                    http.method = %method,
                    http.url = %telemetry_url(url),
                    backend,
                    wait_ms,
                    max_concurrency = blob_transport_max_concurrency() as u64,
                    "remote blob transport queued"
                );
            }
            metrics::record_blob_transport_wait(
                method,
                backend,
                wait_duration.as_secs_f64() * 1000.0,
            );
            Some(permit)
        } else {
            None
        };

        let client = if is_internal {
            &self.internal_client
        } else {
            &self.client
        };
        let mut builder = match method.to_uppercase().as_str() {
            "GET" => client.get(url),
            "POST" => client.post(url),
            "PUT" => client.put(url),
            "DELETE" => client.delete(url),
            "PATCH" => client.patch(url),
            other => return Err(format!("unsupported HTTP method: {other}")),
        };

        for (k, v) in &outbound_headers {
            builder = builder.header(k.as_str(), v.as_str());
        }

        builder = self.add_internal_temper_headers(builder, method, url, &outbound_headers)?;

        if !outbound_headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("traceparent"))
            && let Some(traceparent) =
                current_traceparent_header(&tracing::Span::current(), self.trace_id.as_deref())
        {
            builder = builder.header("traceparent", traceparent);
        }

        if !body.is_empty() {
            builder = builder.body(body.to_vec());
        }

        let resp = builder.send().await.map_err(|e| {
            tracing::warn!(
                error = %e,
                duration_ms = started.elapsed().as_millis() as u64,
                "WASM host binary HTTP request failed"
            );
            format!("HTTP binary request failed: {e}")
        })?;
        let status = resp.status().as_u16();
        let resp_bytes = resp.bytes().await.map_err(|e| {
            tracing::warn!(
                status_code = status,
                error = %e,
                duration_ms = started.elapsed().as_millis() as u64,
                "WASM host binary response read failed"
            );
            format!("failed to read binary response body: {e}")
        })?;
        let resp_bytes = resp_bytes.to_vec();
        let duration_ms = started.elapsed().as_millis() as u64;
        tracing::Span::current().record("status_code", status);
        tracing::Span::current().record("response_bytes", resp_bytes.len() as u64);
        tracing::Span::current().record("duration_ms", duration_ms);
        metrics::record_host_http_call(
            method,
            "binary",
            status,
            body.len() as u64,
            resp_bytes.len() as u64,
            duration_ms as f64,
        );
        Ok((status, resp_bytes))
    }

    async fn connect_call(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<Vec<String>, String> {
        let started = Instant::now();
        let (filtered_headers, mut span_hints) = split_span_hint_headers(headers);
        // ARN-243: drop LLM content from span hints unless this tenant opted in.
        redact_llm_content_hints(&mut span_hints, self.export_llm_content);
        let (is_internal, outbound_headers) = self.outbound_headers(url, &filtered_headers);
        let span = tracing::info_span!(
            "wasm.host.connect_call",
            otel.name = %span_hint_otel_name(&span_hints, "wasm.host.connect_call"),
            http.method = "POST",
            http.url = %telemetry_url(url),
            request_bytes = body.len() as u64,
            header_count = outbound_headers.len() as u64,
            status_code = tracing::field::Empty,
            response_frames = tracing::field::Empty,
            response_bytes = tracing::field::Empty,
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
            dd_llmobs_enabled = tracing::field::Empty,
        );
        record_invocation_context_on_span(&span, self.invocation_context.as_ref(), "connect_call");
        apply_span_hints(&span, &span_hints);
        let _guard = span.enter();

        let client = if is_internal {
            &self.internal_client
        } else {
            &self.client
        };
        let mut builder = client.post(url);

        // Set Connect protocol headers.
        // Use application/connect+json for envd-compatible services (E2B, etc.)
        builder = builder
            .header("content-type", "application/connect+json")
            .header("connect-protocol-version", "1");

        for (k, v) in &outbound_headers {
            builder = builder.header(k.as_str(), v.as_str());
        }
        builder = self.add_internal_temper_headers(builder, "POST", url, &outbound_headers)?;
        if !outbound_headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("traceparent"))
            && let Some(traceparent) =
                current_traceparent_header(&tracing::Span::current(), self.trace_id.as_deref())
        {
            builder = builder.header("traceparent", traceparent);
        }

        if !body.is_empty() {
            builder = builder.body(encode_connect_json_frame(body));
        }

        let resp = builder.send().await.map_err(|e| {
            tracing::Span::current().record("duration_ms", started.elapsed().as_millis() as u64);
            format!("Connect call failed: {e}")
        })?;

        let status = resp.status().as_u16();
        tracing::Span::current().record("status_code", status);
        if !(200..300).contains(&status) {
            let err_body = resp.text().await.unwrap_or_default();
            tracing::Span::current().record("duration_ms", started.elapsed().as_millis() as u64);
            tracing::Span::current().record("response_bytes", err_body.len() as u64);
            return Err(format!("Connect call failed (HTTP {status}): {err_body}"));
        }

        let resp_bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("failed to read Connect response body: {e}"))?;

        let frames = parse_connect_frames(&resp_bytes)?;
        tracing::Span::current().record("response_frames", frames.len() as u64);
        tracing::Span::current().record("response_bytes", resp_bytes.len() as u64);
        tracing::Span::current().record("duration_ms", started.elapsed().as_millis() as u64);
        metrics::record_host_http_call(
            "POST",
            "connect",
            status,
            body.len() as u64,
            resp_bytes.len() as u64,
            started.elapsed().as_secs_f64() * 1000.0,
        );
        Ok(frames)
    }

    fn get_secret(&self, key: &str) -> Result<String, String> {
        let started = Instant::now();
        let span = tracing::info_span!(
            "wasm.host.get_secret",
            otel.name = "wasm.host.get_secret",
            secret.key = %key,
            success = tracing::field::Empty,
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
            dd_llmobs_enabled = tracing::field::Empty,
            error.type = tracing::field::Empty,
            error.message = tracing::field::Empty,
        );
        record_invocation_context_on_span(&span, self.invocation_context.as_ref(), "secret_lookup");
        let _guard = span.enter();
        let result = if key.eq_ignore_ascii_case("temper_api_key") {
            Err("secret 'temper_api_key' is reserved and unavailable to WASM guests".to_string())
        } else {
            self.secret_resolver
                .as_ref()
                .map(|resolver| resolver(key))
                .unwrap_or_else(|| {
                    self.secrets
                        .get(key)
                        .cloned()
                        .ok_or_else(|| format!("secret not found: {key}"))
                })
        };
        tracing::Span::current().record("success", result.is_ok());
        tracing::Span::current().record("duration_ms", started.elapsed().as_millis() as u64);
        if let Err(error) = &result {
            tracing::Span::current().record("error.type", "secret_not_found");
            tracing::Span::current().record("error.message", error.as_str());
        }
        result
    }

    fn log(&self, level: &str, message: &str) {
        record_guest_log_span_event(level, message, self.invocation_context.as_ref(), None);
    }

    fn evaluate_spec(
        &self,
        ioa_source: &str,
        current_state: &str,
        action: &str,
        params_json: &str,
    ) -> Result<String, String> {
        let started = Instant::now();
        let span = tracing::info_span!(
            "wasm.host.evaluate_spec",
            otel.name = "wasm.host.evaluate_spec",
            spec.bytes = ioa_source.len() as u64,
            spec.current_state = %current_state,
            spec.action = %action,
            params_bytes = params_json.len() as u64,
            success = tracing::field::Empty,
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
            dd_llmobs_enabled = tracing::field::Empty,
            error.type = tracing::field::Empty,
            error.message = tracing::field::Empty,
        );
        record_invocation_context_on_span(&span, self.invocation_context.as_ref(), "evaluate_spec");
        let _guard = span.enter();
        let result = match &self.spec_evaluator {
            Some(evaluator) => evaluator(ioa_source, current_state, action, params_json),
            None => Err("evaluate_spec not supported by this host".to_string()),
        };
        tracing::Span::current().record("success", result.is_ok());
        tracing::Span::current().record("duration_ms", started.elapsed().as_millis() as u64);
        if let Err(error) = &result {
            tracing::Span::current().record("error.type", "spec_evaluation_error");
            tracing::Span::current().record("error.message", error.as_str());
        }
        result
    }

    fn emit_progress(&self, event_json: &str) -> Result<(), String> {
        let result = match &self.progress_emitter {
            Some(emitter) => emitter(event_json),
            None => Ok(()),
        };
        guest_progress::record_guest_progress_event(
            event_json,
            self.invocation_context.as_ref(),
            result.as_ref().err().map(String::as_str),
        );
        result
    }

    fn emit_wide_event(&self, event_json: &str) -> Result<(), String> {
        let event = self.build_guest_wide_event(event_json)?;
        wide_event::emit_span(&event);
        wide_event::emit_metrics(&event);
        Ok(())
    }

    fn log_structured(&self, log_json: &str) -> Result<(), String> {
        let payload: GuestStructuredLogInput = serde_json::from_str(log_json)
            .map_err(|e| format!("invalid guest structured log payload: {e}"))?;
        let fields_json = serde_json::to_string(&payload.fields)
            .map_err(|e| format!("structured log fields serialize: {e}"))?;
        let tenant = self
            .invocation_context
            .as_ref()
            .map(|ctx| ctx.tenant.as_str())
            .unwrap_or("");
        let entity_type = self
            .invocation_context
            .as_ref()
            .map(|ctx| ctx.entity_type.as_str())
            .unwrap_or("");
        let entity_id = self
            .invocation_context
            .as_ref()
            .map(|ctx| ctx.entity_id.as_str())
            .unwrap_or("");
        let trigger_action = self
            .invocation_context
            .as_ref()
            .map(|ctx| ctx.trigger_action.as_str())
            .unwrap_or("");
        let session_id = self
            .invocation_context
            .as_ref()
            .and_then(context_session_id)
            .unwrap_or("");
        let wasm_module = self
            .invocation_context
            .as_ref()
            .and_then(|ctx| ctx.wasm_module.as_deref())
            .unwrap_or("");
        let workflow_run_id = self
            .invocation_context
            .as_ref()
            .and_then(|ctx| ctx.workflow_run_id.as_deref())
            .unwrap_or("");
        let trace_id = self.trace_id.as_deref().unwrap_or("");

        record_guest_log_span_event(
            &payload.level,
            &payload.message,
            self.invocation_context.as_ref(),
            Some(&fields_json),
        );

        match payload.level.as_str() {
            "error" => tracing::error!(
                target: "wasm_guest",
                tenant,
                entity_type,
                entity_id,
                trigger_action,
                action_name = trigger_action,
                session_id,
                wasm_module,
                workflow_run_id,
                trace_id,
                fields_json = %fields_json,
                "{}",
                payload.message
            ),
            "warn" => tracing::warn!(
                target: "wasm_guest",
                tenant,
                entity_type,
                entity_id,
                trigger_action,
                action_name = trigger_action,
                session_id,
                wasm_module,
                workflow_run_id,
                trace_id,
                fields_json = %fields_json,
                "{}",
                payload.message
            ),
            "info" => tracing::info!(
                target: "wasm_guest",
                tenant,
                entity_type,
                entity_id,
                trigger_action,
                action_name = trigger_action,
                session_id,
                wasm_module,
                workflow_run_id,
                trace_id,
                fields_json = %fields_json,
                "{}",
                payload.message
            ),
            _ => tracing::debug!(
                target: "wasm_guest",
                tenant,
                entity_type,
                entity_id,
                trigger_action,
                action_name = trigger_action,
                session_id,
                wasm_module,
                workflow_run_id,
                trace_id,
                fields_json = %fields_json,
                "{}",
                payload.message
            ),
        }
        Ok(())
    }

    fn emit_metric(&self, metric_json: &str) -> Result<(), String> {
        let mut payload: GuestMetricInput = serde_json::from_str(metric_json)
            .map_err(|e| format!("invalid guest metric payload: {e}"))?;
        // ADR-0166: guest metric tags are guest-named *and* guest-valued strings,
        // and they reach two sinks below — the span event and the OTel meter.
        // Redact before either reads them, so neither can observe the raw tags.
        redact_guest_string_tags(&mut payload.tags, self.export_llm_content);
        record_guest_metric_span_event(&payload, self.invocation_context.as_ref());
        let meter = opentelemetry::global::meter("temper");
        let mut attrs: Vec<opentelemetry::KeyValue> = payload
            .tags
            .iter()
            .filter(|(key, _)| guest_metric_tag_allowed(key))
            .map(|(key, value)| opentelemetry::KeyValue::new(key.clone(), value.clone()))
            .collect();
        if let Some(ctx) = &self.invocation_context {
            attrs.push(opentelemetry::KeyValue::new("tenant", ctx.tenant.clone()));
            attrs.push(opentelemetry::KeyValue::new(
                "entity_type",
                ctx.entity_type.clone(),
            ));
            attrs.push(opentelemetry::KeyValue::new(
                "trigger_action",
                ctx.trigger_action.clone(),
            ));
            if let Some(module) = &ctx.wasm_module {
                attrs.push(opentelemetry::KeyValue::new("wasm_module", module.clone()));
            }
        }

        if guest_metric_is_counter_kind(payload.kind.as_deref()) {
            meter
                .f64_counter(payload.name)
                .build()
                .add(payload.value, &attrs);
        } else {
            meter
                .f64_histogram(payload.name)
                .build()
                .record(payload.value, &attrs);
        }
        Ok(())
    }

    // --- ADR-0057 streaming primitive (outbound, Phase 1) ---
    //
    // Flow on begin_outbound:
    //   1. Registry mints an OutboundExchange — two paired mpsc
    //      channels and a oneshot for the response head.
    //   2. Spawn a tokio task that bridges to reqwest:
    //        * Reads request body chunks from bridge_request_body,
    //          feeds them into reqwest::Body::wrap_stream.
    //        * Sends the HTTP request.
    //        * On response, fires the head oneshot, then streams
    //          response.bytes_stream() into bridge_response_body.
    //        * Closes both bridge handles on completion / error.
    //   3. Return guest_handles to the guest.
    //
    // Guest sees the handle pair and the head delivery promise; the
    // bridge task owns the reqwest call for its entire lifetime.

    async fn http_stream_begin_outbound(
        &self,
        request: crate::http_stream::HttpRequestHead,
    ) -> Result<crate::http_stream::HttpStreamHandles, String> {
        use futures_util::StreamExt;
        use tracing::Instrument;

        let exchange = self.http_streams.open_outbound_exchange().await;
        let guest = exchange.guest_handles();
        let bridge_req = exchange.bridge_request_body;
        let bridge_resp = exchange.bridge_response_body;
        let head_tx = exchange.bridge_head_sender;
        let streams = self.http_streams.clone();
        let (filtered_headers, mut span_hints) = split_span_hint_headers(&request.headers);
        // ARN-243: drop LLM content from span hints unless this tenant opted in.
        redact_llm_content_hints(&mut span_hints, self.export_llm_content);
        let (is_internal, outbound_headers) =
            self.outbound_headers(&request.url, &filtered_headers);
        let client = if is_internal {
            self.internal_client.clone()
        } else {
            self.client.clone()
        };
        let span = tracing::info_span!(
            "wasm.host.http_stream",
            otel.name = %span_hint_otel_name(&span_hints, "wasm.host.http_stream"),
            http.method = %request.method,
            http.url = %telemetry_url(&request.url),
            header_count = outbound_headers.len() as u64,
            status_code = tracing::field::Empty,
            response_bytes = tracing::field::Empty,
            duration_ms = tracing::field::Empty,
            tenant = tracing::field::Empty,
            wasm_module = tracing::field::Empty,
            observability_event = tracing::field::Empty,
            session_id = tracing::field::Empty,
            managed_session_id = tracing::field::Empty,
            inner_session_id = tracing::field::Empty,
            parent_session_id = tracing::field::Empty,
            agent_id = tracing::field::Empty,
            environment_id = tracing::field::Empty,
            entity_type = tracing::field::Empty,
            entity_id = tracing::field::Empty,
            trigger_action = tracing::field::Empty,
            action_name = tracing::field::Empty,
            workflow_step = tracing::field::Empty,
            workflow_root_entity_type = tracing::field::Empty,
            workflow_root_entity_id = tracing::field::Empty,
            workflow_run_id = tracing::field::Empty,
            dd_llmobs_enabled = tracing::field::Empty,
            tool.name = tracing::field::Empty,
            tool.call_id = tracing::field::Empty,
            gen_ai.operation.name = tracing::field::Empty,
            gen_ai.provider.name = tracing::field::Empty,
            gen_ai.system = tracing::field::Empty,
            gen_ai.request.model = tracing::field::Empty,
            gen_ai.response.model = tracing::field::Empty,
            gen_ai.request.temperature = tracing::field::Empty,
            gen_ai.request.max_tokens = tracing::field::Empty,
            gen_ai.conversation.id = tracing::field::Empty,
            gen_ai.system_instructions = tracing::field::Empty,
            gen_ai.input.messages = tracing::field::Empty,
            gen_ai.prompt = tracing::field::Empty,
            gen_ai.output.messages = tracing::field::Empty,
            gen_ai.completion = tracing::field::Empty,
            gen_ai.response.finish_reasons = tracing::field::Empty,
            gen_ai.usage.input_tokens = tracing::field::Empty,
            gen_ai.usage.output_tokens = tracing::field::Empty,
            gen_ai.usage.cache_read_input_tokens = tracing::field::Empty,
            gen_ai.usage.cache_creation_input_tokens = tracing::field::Empty,
            http.response.body.size = tracing::field::Empty,
        );
        record_invocation_context_on_span(&span, self.invocation_context.as_ref(), "http_stream");
        apply_span_hints(&span, &span_hints);

        // Build the request head before moving into the task so we
        // can surface malformed-method errors synchronously.
        let builder = match request.method.to_uppercase().as_str() {
            "GET" => client.get(&request.url),
            "POST" => client.post(&request.url),
            "PUT" => client.put(&request.url),
            "DELETE" => client.delete(&request.url),
            "PATCH" => client.patch(&request.url),
            other => return Err(format!("unsupported HTTP method: {other}")),
        };
        let mut builder = builder;
        for (k, v) in &outbound_headers {
            builder = builder.header(k.as_str(), v.as_str());
        }
        builder = self.add_internal_temper_headers(
            builder,
            &request.method,
            &request.url,
            &outbound_headers,
        )?;
        if !outbound_headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("traceparent"))
            && let Some(traceparent) = current_traceparent_header(&span, self.trace_id.as_deref())
        {
            builder = builder.header("traceparent", traceparent);
        }

        tokio::spawn(
            async move {
                let started = Instant::now();
                // Pull request body chunks from the registry; wrap as a
                // Stream<Item = Result<Bytes, _>> for reqwest.
                let req_streams = streams.clone();
                let body_stream = async_stream::stream! {
                    loop {
                        match req_streams.read(bridge_req).await {
                            Ok(chunk) if chunk.is_empty() => break, // EOF
                            Ok(chunk) => yield Ok::<_, std::io::Error>(bytes::Bytes::from(chunk)),
                            Err(_) => break,
                        }
                    }
                };
                let req_body = reqwest::Body::wrap_stream(body_stream);

                let send_result = builder.body(req_body).send().await;
                let resp = match send_result {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::Span::current().record("status_code", 0u64);
                        tracing::Span::current()
                            .record("duration_ms", started.elapsed().as_millis() as u64);
                        let _ = head_tx.send(crate::http_stream::HttpResponseHead {
                            status: 0,
                            headers: vec![("x-temper-stream-error".into(), format!("{e}"))],
                        });
                        let _ = streams.close(bridge_resp).await;
                        return;
                    }
                };

                let status = resp.status().as_u16();
                tracing::Span::current().record("status_code", status);
                let headers: Vec<(String, String)> = resp
                    .headers()
                    .iter()
                    .filter_map(|(k, v)| {
                        v.to_str()
                            .ok()
                            .map(|s| (k.as_str().to_string(), s.to_string()))
                    })
                    .collect();
                let _ = head_tx.send(crate::http_stream::HttpResponseHead { status, headers });

                let mut body_stream = resp.bytes_stream();
                let mut response_bytes = 0u64;
                while let Some(chunk) = body_stream.next().await {
                    match chunk {
                        Ok(bytes) => {
                            response_bytes += bytes.len() as u64;
                            if streams.write(bridge_resp, bytes.to_vec()).await.is_err() {
                                break; // guest hung up
                            }
                        }
                        Err(_) => break,
                    }
                }
                tracing::Span::current().record("response_bytes", response_bytes);
                tracing::Span::current()
                    .record("duration_ms", started.elapsed().as_millis() as u64);
                let _ = streams.close(bridge_resp).await;
            }
            .instrument(span),
        );

        Ok(guest)
    }

    async fn http_stream_read(
        &self,
        handle: crate::http_stream::StreamHandle,
    ) -> Result<Vec<u8>, crate::http_stream::StreamError> {
        self.http_streams.read(handle).await
    }

    async fn http_stream_read_bounded(
        &self,
        handle: crate::http_stream::StreamHandle,
        max_bytes: usize,
    ) -> Result<Vec<u8>, crate::http_stream::StreamError> {
        self.http_streams.read_bounded(handle, max_bytes).await
    }

    async fn http_stream_try_write(
        &self,
        handle: crate::http_stream::StreamHandle,
        chunk: Vec<u8>,
    ) -> Result<usize, crate::http_stream::StreamError> {
        self.http_streams.try_write(handle, chunk).await
    }

    async fn http_stream_close(
        &self,
        handle: crate::http_stream::StreamHandle,
    ) -> Result<(), crate::http_stream::StreamError> {
        self.http_streams.close(handle).await
    }

    async fn http_stream_response_head(
        &self,
        response_body: crate::http_stream::StreamHandle,
    ) -> Result<crate::http_stream::HttpResponseHead, String> {
        self.http_streams
            .await_response_head(response_body)
            .await
            .map_err(|e| e.to_string())
    }

    async fn http_stream_send_response_head(
        &self,
        response_body: crate::http_stream::StreamHandle,
        head: crate::http_stream::HttpResponseHead,
    ) -> Result<(), crate::http_stream::StreamError> {
        self.http_streams
            .submit_inbound_response_head(response_body, head)
            .await
    }
}

fn infer_guest_operation(kind: EventKind, tags: &BTreeMap<String, String>) -> Option<String> {
    match kind {
        EventKind::ToolCall => tags
            .get("gen_ai.operation.name")
            .cloned()
            .or_else(|| Some("execute_tool".to_string())),
        EventKind::LlmCall => tags
            .get("gen_ai.operation.name")
            .cloned()
            .or_else(|| Some("chat".to_string())),
        _ => tags.get("operation").cloned(),
    }
}

fn guest_log_span_attrs(
    level: &str,
    message: &str,
    context: Option<&WasmInvocationContext>,
    fields_json: Option<&str>,
) -> BTreeMap<String, String> {
    let mut attrs = BTreeMap::new();
    attrs.insert("log.severity".to_string(), level.to_string());
    attrs.insert("log.message".to_string(), message.to_string());
    attrs.insert("log.target".to_string(), "wasm_guest".to_string());
    if let Some(context) = context {
        attrs.insert("tenant".to_string(), context.tenant.clone());
        attrs.insert("entity_type".to_string(), context.entity_type.clone());
        attrs.insert("entity_id".to_string(), context.entity_id.clone());
        attrs.insert("trigger_action".to_string(), context.trigger_action.clone());
        if let Some(agent_id) = context
            .agent_id
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            attrs.insert("agent_id".to_string(), agent_id.to_string());
        }
        if let Some(module) = context
            .wasm_module
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            attrs.insert("wasm_module".to_string(), module.to_string());
        }
        if let Some(session_id) = context_session_id(context) {
            attrs.insert("session_id".to_string(), session_id.to_string());
            attrs.insert("gen_ai.conversation.id".to_string(), session_id.to_string());
        }
        if let Some(root_type) = context
            .workflow_root_entity_type
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            attrs.insert(
                "workflow_root_entity_type".to_string(),
                root_type.to_string(),
            );
        }
        if let Some(root_id) = context
            .workflow_root_entity_id
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            attrs.insert("workflow_root_entity_id".to_string(), root_id.to_string());
        }
        if let Some(run_id) = context
            .workflow_run_id
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            attrs.insert("workflow_run_id".to_string(), run_id.to_string());
        }
        if !context.trace_id.is_empty() {
            attrs.insert("trace_id".to_string(), context.trace_id.clone());
        }
    }
    if let Some(fields_json) = fields_json.filter(|value| !value.is_empty()) {
        attrs.insert("fields_json".to_string(), fields_json.to_string());
    }
    attrs
}

fn record_guest_log_span_event(
    level: &str,
    message: &str,
    context: Option<&WasmInvocationContext>,
    fields_json: Option<&str>,
) {
    use opentelemetry::KeyValue;
    use opentelemetry::trace::TraceContextExt as _;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    let attrs = guest_log_span_attrs(level, message, context, fields_json)
        .into_iter()
        .map(|(key, value)| KeyValue::new(key, value))
        .collect::<Vec<_>>();
    emit_named_guest_log_span_event(level, message, context, fields_json);
    let span = tracing::Span::current();
    let cx = span.context();
    let otel_span = cx.span();
    if otel_span.span_context().is_valid() {
        otel_span.add_event("wasm_guest.log", attrs);
    }
}

fn record_guest_metric_span_event(
    payload: &GuestMetricInput,
    context: Option<&WasmInvocationContext>,
) {
    use opentelemetry::KeyValue;
    use opentelemetry::trace::TraceContextExt as _;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    let mut attrs = vec![
        KeyValue::new("metric.name", truncate_for_span_attr(&payload.name)),
        KeyValue::new("metric.value", payload.value),
    ];
    if let Some(kind) = payload.kind.as_deref().filter(|value| !value.is_empty()) {
        attrs.push(KeyValue::new("metric.kind", truncate_for_span_attr(kind)));
    }
    if let Some(context) = context {
        attrs.push(KeyValue::new("tenant", context.tenant.clone()));
        attrs.push(KeyValue::new("entity_type", context.entity_type.clone()));
        attrs.push(KeyValue::new("entity_id", context.entity_id.clone()));
        attrs.push(KeyValue::new(
            "trigger_action",
            context.trigger_action.clone(),
        ));
        if let Some(module) = context
            .wasm_module
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            attrs.push(KeyValue::new("wasm_module", module.to_string()));
        }
        if let Some(session_id) = context_session_id(context) {
            attrs.push(KeyValue::new("session_id", session_id.to_string()));
        }
        if let Some(run_id) = context
            .workflow_run_id
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            attrs.push(KeyValue::new("workflow_run_id", run_id.to_string()));
        }
    }
    for (key, value) in &payload.tags {
        attrs.push(KeyValue::new(
            format!("metric.tag.{key}"),
            truncate_for_span_attr(value),
        ));
    }

    let span = tracing::Span::current();
    let cx = span.context();
    let otel_span = cx.span();
    if otel_span.span_context().is_valid() {
        otel_span.add_event("wasm_guest.metric", attrs);
    }
}

fn emit_named_guest_log_span_event(
    level: &str,
    message: &str,
    context: Option<&WasmInvocationContext>,
    fields_json: Option<&str>,
) {
    let tenant = context.map(|ctx| ctx.tenant.as_str()).unwrap_or("");
    let entity_type = context.map(|ctx| ctx.entity_type.as_str()).unwrap_or("");
    let entity_id = context.map(|ctx| ctx.entity_id.as_str()).unwrap_or("");
    let trigger_action = context.map(|ctx| ctx.trigger_action.as_str()).unwrap_or("");
    let agent_id = context
        .and_then(|ctx| ctx.agent_id.as_deref())
        .unwrap_or("");
    let wasm_module = context
        .and_then(|ctx| ctx.wasm_module.as_deref())
        .unwrap_or("");
    let session_id = context.and_then(context_session_id).unwrap_or("");
    let workflow_root_entity_type = context
        .and_then(|ctx| ctx.workflow_root_entity_type.as_deref())
        .unwrap_or("");
    let workflow_root_entity_id = context
        .and_then(|ctx| ctx.workflow_root_entity_id.as_deref())
        .unwrap_or("");
    let workflow_run_id = context
        .and_then(|ctx| ctx.workflow_run_id.as_deref())
        .unwrap_or("");
    let fallback_trace_id = context.map(|ctx| ctx.trace_id.as_str());
    let correlation = current_log_correlation(fallback_trace_id);
    let trace_id = correlation.trace_id.as_str();
    let span_id = correlation.span_id.as_str();
    let dd_trace_id = correlation.dd_trace_id.as_str();
    let dd_span_id = correlation.dd_span_id.as_str();
    let fields_json = fields_json.unwrap_or("");

    match level.to_ascii_lowercase().as_str() {
        "error" => tracing::event!(
            name: "wasm_guest.log",
            target: "wasm_guest",
            tracing::Level::ERROR,
            guest_log.message = %message,
            guest_log.severity = %level,
            guest_log.target = "wasm_guest",
            tenant = %tenant,
            entity_type = %entity_type,
            entity_id = %entity_id,
            trigger_action = %trigger_action,
            agent_id = %agent_id,
            wasm_module = %wasm_module,
            session_id = %session_id,
            gen_ai.conversation.id = %session_id,
            workflow_root_entity_type = %workflow_root_entity_type,
            workflow_root_entity_id = %workflow_root_entity_id,
            workflow_run_id = %workflow_run_id,
            trace_id = %trace_id,
            span_id = %span_id,
            otel.trace_id = %trace_id,
            otel.span_id = %span_id,
            dd.trace_id = %dd_trace_id,
            dd.span_id = %dd_span_id,
            fields_json = %fields_json,
            "{}",
            message,
        ),
        "warn" => tracing::event!(
            name: "wasm_guest.log",
            target: "wasm_guest",
            tracing::Level::WARN,
            guest_log.message = %message,
            guest_log.severity = %level,
            guest_log.target = "wasm_guest",
            tenant = %tenant,
            entity_type = %entity_type,
            entity_id = %entity_id,
            trigger_action = %trigger_action,
            agent_id = %agent_id,
            wasm_module = %wasm_module,
            session_id = %session_id,
            gen_ai.conversation.id = %session_id,
            workflow_root_entity_type = %workflow_root_entity_type,
            workflow_root_entity_id = %workflow_root_entity_id,
            workflow_run_id = %workflow_run_id,
            trace_id = %trace_id,
            span_id = %span_id,
            otel.trace_id = %trace_id,
            otel.span_id = %span_id,
            dd.trace_id = %dd_trace_id,
            dd.span_id = %dd_span_id,
            fields_json = %fields_json,
            "{}",
            message,
        ),
        "info" => tracing::event!(
            name: "wasm_guest.log",
            target: "wasm_guest",
            tracing::Level::INFO,
            guest_log.message = %message,
            guest_log.severity = %level,
            guest_log.target = "wasm_guest",
            tenant = %tenant,
            entity_type = %entity_type,
            entity_id = %entity_id,
            trigger_action = %trigger_action,
            agent_id = %agent_id,
            wasm_module = %wasm_module,
            session_id = %session_id,
            gen_ai.conversation.id = %session_id,
            workflow_root_entity_type = %workflow_root_entity_type,
            workflow_root_entity_id = %workflow_root_entity_id,
            workflow_run_id = %workflow_run_id,
            trace_id = %trace_id,
            span_id = %span_id,
            otel.trace_id = %trace_id,
            otel.span_id = %span_id,
            dd.trace_id = %dd_trace_id,
            dd.span_id = %dd_span_id,
            fields_json = %fields_json,
            "{}",
            message,
        ),
        _ => tracing::event!(
            name: "wasm_guest.log",
            target: "wasm_guest",
            tracing::Level::DEBUG,
            guest_log.message = %message,
            guest_log.severity = %level,
            guest_log.target = "wasm_guest",
            tenant = %tenant,
            entity_type = %entity_type,
            entity_id = %entity_id,
            trigger_action = %trigger_action,
            agent_id = %agent_id,
            wasm_module = %wasm_module,
            session_id = %session_id,
            gen_ai.conversation.id = %session_id,
            workflow_root_entity_type = %workflow_root_entity_type,
            workflow_root_entity_id = %workflow_root_entity_id,
            workflow_run_id = %workflow_run_id,
            trace_id = %trace_id,
            span_id = %span_id,
            otel.trace_id = %trace_id,
            otel.span_id = %span_id,
            dd.trace_id = %dd_trace_id,
            dd.span_id = %dd_span_id,
            fields_json = %fields_json,
            "{}",
            message,
        ),
    }
}

fn context_session_id(context: &WasmInvocationContext) -> Option<&str> {
    context
        .session_id
        .as_deref()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            (context.entity_type == "Session" && !context.entity_id.is_empty())
                .then_some(context.entity_id.as_str())
        })
}

fn record_invocation_context_on_span(
    span: &tracing::Span,
    context: Option<&WasmInvocationContext>,
    workflow_step: &str,
) {
    span.record("workflow_step", workflow_step);
    let Some(context) = context else {
        return;
    };
    span.record("tenant", context.tenant.as_str());
    span.record("entity_type", context.entity_type.as_str());
    span.record("entity_id", context.entity_id.as_str());
    span.record("trigger_action", context.trigger_action.as_str());
    span.record("action_name", context.trigger_action.as_str());
    if let Some(module) = context
        .wasm_module
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        span.record("wasm_module", module);
    }
    if let Some(agent_id) = context
        .agent_id
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        span.record("agent_id", agent_id);
    }
    if let Some(session_id) = context_session_id(context) {
        span.record("session_id", session_id);
    }
    if let Some(root_type) = context
        .workflow_root_entity_type
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        span.record("workflow_root_entity_type", root_type);
    }
    if let Some(root_id) = context
        .workflow_root_entity_id
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        span.record("workflow_root_entity_id", root_id);
    }
    if let Some(run_id) = context
        .workflow_run_id
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        span.record("workflow_run_id", run_id);
    }
}

#[derive(Default)]
struct LogCorrelation {
    trace_id: String,
    span_id: String,
    dd_trace_id: String,
    dd_span_id: String,
}

fn current_log_correlation(fallback_trace_id: Option<&str>) -> LogCorrelation {
    use opentelemetry::trace::TraceContextExt as _;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    let span = tracing::Span::current();
    let cx = span.context();
    let span_context = cx.span().span_context().clone();
    let (trace_id, span_id) = if span_context.is_valid() {
        (
            span_context.trace_id().to_string(),
            span_context.span_id().to_string(),
        )
    } else {
        (
            fallback_trace_id.map(str::to_string).unwrap_or_default(),
            String::new(),
        )
    };
    let dd_trace_id = datadog_trace_id_decimal(&trace_id).unwrap_or_default();
    let dd_span_id = datadog_span_id_decimal(&span_id).unwrap_or_default();
    LogCorrelation {
        trace_id,
        span_id,
        dd_trace_id,
        dd_span_id,
    }
}

fn datadog_trace_id_decimal(trace_id: &str) -> Option<String> {
    let trace_id = trace_id.trim();
    if trace_id.is_empty() {
        return None;
    }
    let low_bits = if trace_id.len() > 16 {
        trace_id.get(trace_id.len() - 16..)?
    } else {
        trace_id
    };
    u64::from_str_radix(low_bits, 16)
        .ok()
        .map(|value| value.to_string())
}

fn datadog_span_id_decimal(span_id: &str) -> Option<String> {
    let span_id = span_id.trim();
    if span_id.is_empty() {
        return None;
    }
    u64::from_str_radix(span_id, 16)
        .ok()
        .map(|value| value.to_string())
}

fn telemetry_url(url: &str) -> String {
    let after_scheme = url.find("://").map(|idx| &url[idx + 3..]).unwrap_or(url);
    let after_auth = after_scheme
        .find('@')
        .map(|idx| &after_scheme[idx + 1..])
        .unwrap_or(after_scheme);
    let path_start = after_auth.find(['/', '?', '#']).unwrap_or(after_auth.len());
    let authority = &after_auth[..path_start];
    let path_and_query = &after_auth[path_start..];
    let path_end = path_and_query
        .find(['?', '#'])
        .unwrap_or(path_and_query.len());
    let path = &path_and_query[..path_end];
    if path.is_empty() {
        authority.to_string()
    } else {
        format!("{authority}{path}")
    }
}

fn current_traceparent_header(
    span: &tracing::Span,
    fallback_trace_id: Option<&str>,
) -> Option<String> {
    use opentelemetry::trace::TraceContextExt as _;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    let span_context = span.context().span().span_context().clone();
    if span_context.is_valid() {
        let flags = if span_context.trace_flags().is_sampled() {
            "01"
        } else {
            "00"
        };
        return Some(format!(
            "00-{}-{}-{}",
            span_context.trace_id(),
            span_context.span_id(),
            flags
        ));
    }

    let trace_id = fallback_trace_id.filter(|value| !value.is_empty())?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let span_id = format!("{nanos:016x}");
    Some(format!("00-{trace_id}-{span_id}-01"))
}

/// Parse Connect protocol binary frames from a response body.
///
/// Each frame has a 5-byte prefix: 1 flag byte + 4 big-endian length bytes.
/// Flag 0x00 = data frame, flag 0x02 = trailer frame (end-of-stream).
/// Returns the payload of all data frames as strings.
pub fn parse_connect_frames(data: &[u8]) -> Result<Vec<String>, String> {
    let mut frames = Vec::new();
    let mut offset = 0;

    while offset < data.len() {
        if offset + 5 > data.len() {
            return Err(format!(
                "incomplete Connect frame header at offset {offset} (need 5 bytes, have {})",
                data.len() - offset
            ));
        }

        let flags = data[offset];
        let length = u32::from_be_bytes([
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
            data[offset + 4],
        ]) as usize;
        offset += 5;

        if offset + length > data.len() {
            return Err(format!(
                "incomplete Connect frame payload at offset {}: expected {length} bytes, have {}",
                offset - 5,
                data.len() - offset
            ));
        }

        let payload = &data[offset..offset + length];
        offset += length;

        // flags 0x00 = data frame, 0x02 = trailer/end-of-stream
        if flags & 0x02 == 0 {
            let payload_str = String::from_utf8(payload.to_vec())
                .map_err(|e| format!("Connect frame payload is not valid UTF-8: {e}"))?;
            frames.push(payload_str);
        }
        // Trailer frames (0x02) are skipped — they contain metadata, not data
    }

    Ok(frames)
}

/// Encode a JSON payload as a Connect protocol envelope.
///
/// Connect JSON still uses the 5-byte envelope framing: 1 flag byte followed by
/// a 4-byte big-endian payload length.
pub fn encode_connect_json_frame(body: &str) -> Vec<u8> {
    let payload = body.as_bytes();
    let mut framed = Vec::with_capacity(5 + payload.len());
    framed.push(0x00);
    framed.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    framed.extend_from_slice(payload);
    framed
}

/// Simulation host: canned responses, captured logs.
///
/// Uses `BTreeMap` for deterministic iteration (DST compliance).
pub struct SimWasmHost {
    /// Canned HTTP responses: URL pattern -> (status, body).
    responses: BTreeMap<String, (u16, String)>,
    /// Canned binary HTTP responses: URL pattern -> (status, bytes).
    binary_responses: BTreeMap<String, (u16, Vec<u8>)>,
    /// Canned Connect responses: URL pattern -> vec of frame payloads.
    connect_responses: BTreeMap<String, Vec<String>>,
    /// Canned secrets.
    secrets: BTreeMap<String, String>,
    /// Canned evaluate_spec responses: (ioa_source_hash, action) -> result JSON.
    spec_eval_responses: BTreeMap<(String, String), String>,
    /// Default response for URLs not in the map.
    default_response: (u16, String),
    /// Default binary response for URLs not in the binary map.
    default_binary_response: (u16, Vec<u8>),
    /// Canned application-data response for deterministic ABI simulation.
    data_response: Option<Vec<u8>>,
    data_request_budget: usize,
    data_response_handle_budget: usize,
}

impl SimWasmHost {
    /// Create a simulation host with default 200 OK responses.
    pub fn new() -> Self {
        Self {
            responses: BTreeMap::new(),
            binary_responses: BTreeMap::new(),
            connect_responses: BTreeMap::new(),
            secrets: BTreeMap::new(),
            spec_eval_responses: BTreeMap::new(),
            default_response: (200, r#"{"ok": true}"#.to_string()),
            default_binary_response: (200, Vec::new()),
            data_response: None,
            data_request_budget: 1024 * 1024,
            data_response_handle_budget: 4,
        }
    }

    /// Add a canned HTTP response for a URL.
    pub fn with_response(mut self, url: &str, status: u16, body: &str) -> Self {
        self.responses
            .insert(url.to_string(), (status, body.to_string()));
        self
    }

    /// Add a canned binary HTTP response for a URL.
    pub fn with_binary_response(mut self, url: &str, status: u16, bytes: Vec<u8>) -> Self {
        self.binary_responses
            .insert(url.to_string(), (status, bytes));
        self
    }

    /// Add a canned Connect response for a URL.
    pub fn with_connect_response(mut self, url: &str, frames: Vec<String>) -> Self {
        self.connect_responses.insert(url.to_string(), frames);
        self
    }

    /// Add a canned secret.
    pub fn with_secret(mut self, key: &str, value: &str) -> Self {
        self.secrets.insert(key.to_string(), value.to_string());
        self
    }

    /// Set the default response for unmatched URLs.
    pub fn with_default_response(mut self, status: u16, body: &str) -> Self {
        self.default_response = (status, body.to_string());
        self
    }

    /// Set the default binary response for unmatched URLs.
    pub fn with_default_binary_response(mut self, status: u16, bytes: Vec<u8>) -> Self {
        self.default_binary_response = (status, bytes);
        self
    }

    /// Add a canned evaluate_spec response for a given action.
    pub fn with_spec_eval_response(
        mut self,
        ioa_hash: &str,
        action: &str,
        result_json: &str,
    ) -> Self {
        self.spec_eval_responses.insert(
            (ioa_hash.to_string(), action.to_string()),
            result_json.to_string(),
        );
        self
    }

    /// Return these bytes from the application-data ABI in simulation.
    pub fn with_data_response(mut self, response: Vec<u8>) -> Self {
        self.data_response = Some(response);
        self
    }

    /// Override deterministic application-data request and response-handle budgets.
    pub fn with_data_budgets(mut self, request_bytes: usize, response_handles: usize) -> Self {
        self.data_request_budget = request_bytes;
        self.data_response_handle_budget = response_handles;
        self
    }
}

impl Default for SimWasmHost {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl WasmHost for SimWasmHost {
    fn temper_data_request_budget(&self) -> usize {
        self.data_request_budget
    }

    fn temper_data_response_handle_budget(&self) -> usize {
        self.data_response_handle_budget
    }

    async fn temper_data_call(&self, _request: &[u8]) -> Result<Vec<u8>, String> {
        self.data_response
            .clone()
            .ok_or_else(|| "no simulated application-data response configured".into())
    }

    async fn http_call(
        &self,
        _method: &str,
        url: &str,
        _headers: &[(String, String)],
        _body: &str,
    ) -> Result<(u16, String), String> {
        let (status, body) = self
            .responses
            .get(url)
            .cloned()
            .unwrap_or_else(|| self.default_response.clone());
        Ok((status, body))
    }

    async fn http_call_binary(
        &self,
        _method: &str,
        url: &str,
        _headers: &[(String, String)],
        _body: &[u8],
    ) -> Result<(u16, Vec<u8>), String> {
        let (status, bytes) = self
            .binary_responses
            .get(url)
            .cloned()
            .unwrap_or_else(|| self.default_binary_response.clone());
        Ok((status, bytes))
    }

    async fn connect_call(
        &self,
        url: &str,
        _headers: &[(String, String)],
        _body: &str,
    ) -> Result<Vec<String>, String> {
        Ok(self.connect_responses.get(url).cloned().unwrap_or_default())
    }

    fn get_secret(&self, key: &str) -> Result<String, String> {
        self.secrets
            .get(key)
            .cloned()
            .ok_or_else(|| format!("sim secret not found: {key}"))
    }

    fn log(&self, level: &str, message: &str) {
        tracing::debug!(target: "wasm_guest_sim", level = level, "{}", message);
    }

    fn evaluate_spec(
        &self,
        ioa_source: &str,
        _current_state: &str,
        action: &str,
        _params_json: &str,
    ) -> Result<String, String> {
        // Use a simple hash of the IOA source for lookup
        let hash = format!("{:x}", ioa_source.len());
        self.spec_eval_responses
            .get(&(hash, action.to_string()))
            .cloned()
            .ok_or_else(|| format!("sim: no canned response for action '{action}'"))
    }

    fn emit_progress(&self, _event_json: &str) -> Result<(), String> {
        Ok(())
    }

    fn emit_wide_event(&self, _event_json: &str) -> Result<(), String> {
        Ok(())
    }

    fn log_structured(&self, _log_json: &str) -> Result<(), String> {
        Ok(())
    }

    fn emit_metric(&self, _metric_json: &str) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
#[path = "host_trait/host_trait_test.rs"]
mod tests;
