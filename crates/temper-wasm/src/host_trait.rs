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
use temper_observe::wide_event::{self, EventKind, WideEvent};

/// Host capabilities provided to WASM modules.
///
/// Production uses real HTTP + secret store. Simulation uses canned
/// responses for deterministic testing.
#[async_trait]
pub trait WasmHost: Send + Sync {
    /// Make an HTTP request. Returns (status_code, response_body).
    async fn http_call(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<(u16, String), String>;

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

/// Production host: real HTTP calls via reqwest, real secrets.
pub struct ProductionWasmHost {
    /// HTTP client for making real requests.
    client: reqwest::Client,
    /// Secrets from env vars or a secret store.
    secrets: BTreeMap<String, String>,
    /// Optional spec evaluator (provided by temper-server at construction).
    spec_evaluator: Option<SpecEvaluatorFn>,
    /// Optional progress emitter (provided by temper-server at construction).
    progress_emitter: Option<ProgressEmitterFn>,
    /// W3C trace ID for auto-injecting traceparent headers in HTTP calls.
    trace_id: Option<String>,
    /// Optional short-circuit for binary HTTP calls.
    binary_http_interceptor: Option<BinaryHttpInterceptorFn>,
    /// Invocation context for auto-enriching guest telemetry.
    invocation_context: Option<WasmInvocationContext>,
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

impl ProductionWasmHost {
    /// Create with pre-loaded secrets and default HTTP timeout.
    ///
    /// The default timeout matches `WasmResourceLimits::default().max_duration`
    /// (120s per ADR-0045).
    pub fn new(secrets: BTreeMap<String, String>) -> Self {
        Self::with_timeout(secrets, crate::WasmResourceLimits::default().max_duration)
    }

    /// Create with pre-loaded secrets and a custom HTTP request timeout.
    ///
    /// Secrets whose key starts with `ca_cert:` are treated as PEM-encoded
    /// CA certificates and added as trusted roots to the HTTP client. This
    /// lets operators provision private CA trust via the same secret store
    /// that WASM modules already use, with no filesystem or env var coupling.
    pub fn with_timeout(secrets: BTreeMap<String, String>, timeout: std::time::Duration) -> Self {
        let mut builder = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(timeout);

        for (key, pem) in &secrets {
            if !key.starts_with("ca_cert:") {
                continue;
            }
            match reqwest::Certificate::from_pem(pem.as_bytes()) {
                Ok(cert) => {
                    tracing::info!(key, "loaded CA certificate from secret store");
                    builder = builder.add_root_certificate(cert);
                }
                Err(e) => {
                    tracing::warn!(key, error = %e, "failed to parse CA certificate from secret");
                }
            }
        }

        Self {
            client: builder.build().unwrap_or_default(),
            secrets,
            spec_evaluator: None,
            progress_emitter: None,
            trace_id: None,
            binary_http_interceptor: None,
            invocation_context: None,
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

    /// Create with a binary HTTP interceptor for local fast paths.
    pub fn with_binary_http_interceptor(mut self, interceptor: BinaryHttpInterceptorFn) -> Self {
        self.binary_http_interceptor = Some(interceptor);
        self
    }

    fn build_guest_wide_event(&self, event_json: &str) -> Result<WideEvent, String> {
        let payload: GuestWideEventInput = serde_json::from_str(event_json)
            .map_err(|e| format!("invalid guest wide event payload: {e}"))?;

        let mut tags = payload.tags;
        let mut attributes = payload.attributes;
        let measurements = payload.measurements;

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
            if let Some(session_id) = &ctx.session_id {
                attributes
                    .entry("gen_ai.conversation.id".into())
                    .or_insert_with(|| json!(session_id));
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
        let (filtered_headers, span_hints) = split_span_hint_headers(headers);
        let span = tracing::info_span!(
            "wasm.host.http_call",
            otel.name = "wasm.host.http_call",
            http.method = %method,
            http.url = %telemetry_url(url),
            request_bytes = body.len() as u64,
            header_count = filtered_headers.len() as u64,
            status_code = tracing::field::Empty,
            response_bytes = tracing::field::Empty,
            duration_ms = tracing::field::Empty,
        );
        let _guard = span.enter();
        apply_span_hints(&tracing::Span::current(), &span_hints);

        let mut builder = match method.to_uppercase().as_str() {
            "GET" => self.client.get(url),
            "POST" => self.client.post(url),
            "PUT" => self.client.put(url),
            "DELETE" => self.client.delete(url),
            "PATCH" => self.client.patch(url),
            other => return Err(format!("unsupported HTTP method: {other}")),
        };

        for (k, v) in &filtered_headers {
            builder = builder.header(k.as_str(), v.as_str());
        }

        // Auto-inject Temper auth headers for internal API calls (ADR-0043).
        // Internal = URL starts with temper_api_url from secrets.
        // Only inject when the guest hasn't already set principal headers,
        // allowing cross-tenant admin calls (e.g. request_approval → temper-system).
        let is_internal = self
            .secrets
            .get("temper_api_url")
            .is_some_and(|api_url| url.starts_with(api_url.trim_end_matches('/')));
        if is_internal && let Some(ref inv_ctx) = self.invocation_context {
            let has_principal = filtered_headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("x-temper-principal-kind"));
            if !has_principal {
                let agent_type = if inv_ctx.entity_type.eq_ignore_ascii_case("Session") {
                    "agent"
                } else {
                    "system"
                };
                let principal_id = inv_ctx
                    .agent_id
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(inv_ctx.entity_id.as_str());
                builder = builder
                    .header("x-tenant-id", inv_ctx.tenant.as_str())
                    .header("x-temper-principal-kind", "agent")
                    .header("x-temper-principal-id", principal_id)
                    .header("x-temper-agent-type", agent_type);
                if let Some(ref sid) = inv_ctx.session_id {
                    builder = builder.header("x-temper-ctx-sessionid", sid.as_str());
                }
                if let Some(key) = self.secrets.get("temper_api_key").filter(|k| !k.is_empty()) {
                    builder = builder.header("authorization", format!("Bearer {key}"));
                }
            }
        }
        // determinism-ok: is_internal check uses non-deterministic URL comparison,
        // but this runs in WasmHost (not simulation), so wall-clock/network access is fine.

        // Auto-inject traceparent for cross-request trace correlation.
        if !filtered_headers
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
        let (filtered_headers, span_hints) = split_span_hint_headers(headers);
        let span = tracing::info_span!(
            "wasm.host.http_call_binary",
            otel.name = "wasm.host.http_call_binary",
            http.method = %method,
            http.url = %telemetry_url(url),
            request_bytes = body.len() as u64,
            header_count = filtered_headers.len() as u64,
            status_code = tracing::field::Empty,
            response_bytes = tracing::field::Empty,
            duration_ms = tracing::field::Empty,
        );
        let _guard = span.enter();
        apply_span_hints(&tracing::Span::current(), &span_hints);

        if let Some(ref interceptor) = self.binary_http_interceptor
            && let Some(result) = interceptor(
                method.to_string(),
                url.to_string(),
                filtered_headers.clone(),
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

        let mut builder = match method.to_uppercase().as_str() {
            "GET" => self.client.get(url),
            "POST" => self.client.post(url),
            "PUT" => self.client.put(url),
            "DELETE" => self.client.delete(url),
            "PATCH" => self.client.patch(url),
            other => return Err(format!("unsupported HTTP method: {other}")),
        };

        for (k, v) in &filtered_headers {
            builder = builder.header(k.as_str(), v.as_str());
        }

        if !headers
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
        let mut builder = self.client.post(url);

        // Set Connect protocol headers.
        // Use application/connect+json for envd-compatible services (E2B, etc.)
        builder = builder
            .header("content-type", "application/connect+json")
            .header("connect-protocol-version", "1");

        for (k, v) in headers {
            builder = builder.header(k.as_str(), v.as_str());
        }

        if !body.is_empty() {
            builder = builder.body(encode_connect_json_frame(body));
        }

        let resp = builder
            .send()
            .await
            .map_err(|e| format!("Connect call failed: {e}"))?;

        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let err_body = resp.text().await.unwrap_or_default();
            return Err(format!("Connect call failed (HTTP {status}): {err_body}"));
        }

        let resp_bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("failed to read Connect response body: {e}"))?;

        parse_connect_frames(&resp_bytes)
    }

    fn get_secret(&self, key: &str) -> Result<String, String> {
        self.secrets
            .get(key)
            .cloned()
            .ok_or_else(|| format!("secret not found: {key}"))
    }

    fn log(&self, level: &str, message: &str) {
        match level {
            "error" => tracing::error!(target: "wasm_guest", "{}", message),
            "warn" => tracing::warn!(target: "wasm_guest", "{}", message),
            "info" => tracing::info!(target: "wasm_guest", "{}", message),
            _ => tracing::debug!(target: "wasm_guest", "{}", message),
        }
    }

    fn evaluate_spec(
        &self,
        ioa_source: &str,
        current_state: &str,
        action: &str,
        params_json: &str,
    ) -> Result<String, String> {
        match &self.spec_evaluator {
            Some(evaluator) => evaluator(ioa_source, current_state, action, params_json),
            None => Err("evaluate_spec not supported by this host".to_string()),
        }
    }

    fn emit_progress(&self, event_json: &str) -> Result<(), String> {
        match &self.progress_emitter {
            Some(emitter) => emitter(event_json),
            None => Ok(()),
        }
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
            .and_then(|ctx| ctx.session_id.as_deref())
            .unwrap_or("");
        let trace_id = self.trace_id.as_deref().unwrap_or("");

        match payload.level.as_str() {
            "error" => tracing::error!(
                target: "wasm_guest",
                tenant,
                entity_type,
                entity_id,
                trigger_action,
                session_id,
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
                session_id,
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
                session_id,
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
                session_id,
                trace_id,
                fields_json = %fields_json,
                "{}",
                payload.message
            ),
        }
        Ok(())
    }

    fn emit_metric(&self, metric_json: &str) -> Result<(), String> {
        let payload: GuestMetricInput = serde_json::from_str(metric_json)
            .map_err(|e| format!("invalid guest metric payload: {e}"))?;
        let meter = opentelemetry::global::meter("temper");
        let mut attrs: Vec<opentelemetry::KeyValue> = payload
            .tags
            .into_iter()
            .map(|(key, value)| opentelemetry::KeyValue::new(key, value))
            .collect();
        if let Some(ctx) = &self.invocation_context {
            attrs.push(opentelemetry::KeyValue::new(
                "entity_type",
                ctx.entity_type.clone(),
            ));
        }

        match payload.kind.as_deref() {
            Some("counter") => {
                meter
                    .f64_counter(payload.name)
                    .build()
                    .add(payload.value, &attrs);
            }
            _ => {
                meter
                    .f64_histogram(payload.name)
                    .build()
                    .record(payload.value, &attrs);
            }
        }
        Ok(())
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

/// Span hints extracted from a WASM HTTP call's headers. See
/// [`split_span_hint_headers`].
#[derive(Default, Debug, PartialEq, Eq)]
pub(crate) struct SpanHints {
    /// If set, override the `wasm.host.http_call` span's name (from
    /// `X-Temper-Span-Name`).
    pub span_name: Option<String>,
    /// Additional span attributes to record (from `X-Temper-Span-Attr-<key>`
    /// headers). Keys are stripped of the prefix; both key and value must be
    /// non-empty.
    pub attributes: Vec<(String, String)>,
}

/// Split a header list into headers forwarded to the upstream request and
/// Temper-specific span hints. See ADR-0037.
///
/// WASM modules (e.g., `llm_caller`, `monty_repl`) can annotate their
/// outgoing HTTP calls with semantically meaningful span names and
/// attributes without introducing a new ABI, by prefixing headers with
/// `X-Temper-Span-`. These headers are consumed by the host and removed
/// from the outbound request.
///
/// Recognized hint headers (case-insensitive):
/// - `X-Temper-Span-Name: <name>` — sets span name (e.g., `tool.llm_call`).
/// - `X-Temper-Span-Attr-<key>: <value>` — adds `<key>=<value>` as a span
///   attribute. Useful for `gen_ai.request.model`, `tool.name`,
///   `tool.call_id`, etc.
///
/// Any `X-Temper-Span-*` header with an unrecognized suffix is stripped
/// (reserved for forward-compat) but otherwise ignored.
pub(crate) fn split_span_hint_headers(
    headers: &[(String, String)],
) -> (Vec<(String, String)>, SpanHints) {
    const PREFIX: &str = "x-temper-span-";
    const NAME_HEADER: &str = "x-temper-span-name";
    const ATTR_PREFIX: &str = "x-temper-span-attr-";

    let mut kept: Vec<(String, String)> = Vec::with_capacity(headers.len());
    let mut hints = SpanHints::default();
    for (k, v) in headers {
        let lk = k.to_ascii_lowercase();
        if lk == NAME_HEADER {
            if !v.is_empty() {
                hints.span_name = Some(v.clone());
            }
            continue;
        }
        if let Some(attr_key) = lk.strip_prefix(ATTR_PREFIX) {
            if !attr_key.is_empty() && !v.is_empty() {
                hints.attributes.push((attr_key.to_string(), v.clone()));
            }
            continue;
        }
        if lk.starts_with(PREFIX) {
            // Unknown X-Temper-Span-* header — strip silently for
            // forward compatibility.
            continue;
        }
        kept.push((k.clone(), v.clone()));
    }
    (kept, hints)
}

/// Apply span hints to the currently-active tracing span's underlying OTel
/// span. `update_name` overrides the span display name; `set_attribute`
/// calls attach the key/value pairs. If no tracer subscriber is installed
/// (tests, early startup), this is a no-op.
pub(crate) fn apply_span_hints(span: &tracing::Span, hints: &SpanHints) {
    use opentelemetry::KeyValue;
    use opentelemetry::trace::TraceContextExt;
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    let cx = span.context();
    let otel_span = cx.span();
    if let Some(ref name) = hints.span_name {
        otel_span.update_name(name.clone());
    }
    for (k, v) in &hints.attributes {
        otel_span.set_attribute(KeyValue::new(k.clone(), v.clone()));
    }
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
}

impl Default for SimWasmHost {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl WasmHost for SimWasmHost {
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
mod tests {
    use super::*;
    use opentelemetry::trace::{TraceContextExt, TracerProvider as _};
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    use tracing_subscriber::prelude::*;

    /// Build a Connect frame: [flags(1)][length(4 big-endian)][payload].
    fn make_frame(flags: u8, payload: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(5 + payload.len());
        frame.push(flags);
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(payload);
        frame
    }

    #[test]
    fn parse_single_data_frame() {
        let payload = b"{\"stdout\":\"hello\"}";
        let data = make_frame(0x00, payload);
        let frames = parse_connect_frames(&data).expect("single data frame should parse");
        assert_eq!(frames, vec!["{\"stdout\":\"hello\"}"]);
    }

    #[test]
    fn parse_multiple_frames() {
        let mut data = make_frame(0x00, b"{\"stdout\":\"line1\"}");
        data.extend(make_frame(0x00, b"{\"stdout\":\"line2\"}"));
        data.extend(make_frame(0x02, b"trailer")); // trailer frame, should be skipped
        let frames = parse_connect_frames(&data).expect("multiple connect frames should parse");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0], "{\"stdout\":\"line1\"}");
        assert_eq!(frames[1], "{\"stdout\":\"line2\"}");
    }

    #[test]
    fn parse_empty_input() {
        let frames = parse_connect_frames(&[]).expect("empty input should parse");
        assert!(frames.is_empty());
    }

    #[test]
    fn encode_connect_json_frame_wraps_payload() {
        let payload = "{\"hello\":\"world\"}";
        let framed = encode_connect_json_frame(payload);
        assert_eq!(framed[0], 0x00);
        assert_eq!(
            u32::from_be_bytes([framed[1], framed[2], framed[3], framed[4]]) as usize,
            payload.len()
        );
        assert_eq!(&framed[5..], payload.as_bytes());
    }

    #[test]
    fn parse_trailer_only() {
        let data = make_frame(0x02, b"{}");
        let frames = parse_connect_frames(&data).expect("trailer-only frame should parse");
        assert!(frames.is_empty());
    }

    #[test]
    fn parse_incomplete_header_errors() {
        let result = parse_connect_frames(&[0x00, 0x00]);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("incomplete Connect frame header")
        );
    }

    #[test]
    fn parse_incomplete_payload_errors() {
        // Header says 100 bytes but only 3 available
        let mut data = vec![0x00];
        data.extend_from_slice(&100u32.to_be_bytes());
        data.extend_from_slice(b"abc");
        let result = parse_connect_frames(&data);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("incomplete Connect frame payload")
        );
    }

    #[test]
    fn current_traceparent_header_prefers_active_span_context() {
        let tracer_provider = SdkTracerProvider::builder().build();
        let subscriber = tracing_subscriber::registry().with(
            tracing_opentelemetry::layer().with_tracer(tracer_provider.tracer("temper-wasm-test")),
        );
        let _subscriber_guard = tracing::subscriber::set_default(subscriber);

        let span = tracing::info_span!("wasm.reply");
        let expected = {
            let _guard = span.enter();
            let span_context = tracing::Span::current()
                .context()
                .span()
                .span_context()
                .clone();
            assert!(
                span_context.is_valid(),
                "test span should have an OTEL context"
            );
            format!(
                "00-{}-{}-01",
                span_context.trace_id(),
                span_context.span_id()
            )
        };

        let actual = span
            .in_scope(|| current_traceparent_header(&tracing::Span::current(), None))
            .expect("active span should produce a traceparent");
        assert_eq!(actual, expected);
    }

    // --- Span-hint-header extraction (ADR-0037) ---

    #[test]
    fn split_span_hint_headers_preserves_regular_headers() {
        let headers = vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("authorization".to_string(), "Bearer xyz".to_string()),
        ];
        let (kept, hints) = split_span_hint_headers(&headers);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].0, "content-type");
        assert_eq!(kept[1].0, "authorization");
        assert!(hints.span_name.is_none());
        assert!(hints.attributes.is_empty());
    }

    #[test]
    fn split_span_hint_headers_extracts_span_name_case_insensitive() {
        let headers = vec![
            (
                "X-Temper-Span-Name".to_string(),
                "tool.anthropic".to_string(),
            ),
            ("content-type".to_string(), "application/json".to_string()),
        ];
        let (kept, hints) = split_span_hint_headers(&headers);
        assert_eq!(hints.span_name.as_deref(), Some("tool.anthropic"));
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].0, "content-type");
    }

    #[test]
    fn split_span_hint_headers_extracts_generic_attributes() {
        let headers = vec![
            (
                "X-Temper-Span-Attr-gen_ai.request.model".to_string(),
                "claude-sonnet-4.6".to_string(),
            ),
            (
                "x-temper-span-attr-tool.name".to_string(),
                "temper_write".to_string(),
            ),
            ("content-type".to_string(), "application/json".to_string()),
        ];
        let (kept, hints) = split_span_hint_headers(&headers);
        assert_eq!(kept.len(), 1);
        assert_eq!(hints.attributes.len(), 2);
        assert!(
            hints
                .attributes
                .iter()
                .any(|(k, v)| k == "gen_ai.request.model" && v == "claude-sonnet-4.6")
        );
        assert!(
            hints
                .attributes
                .iter()
                .any(|(k, v)| k == "tool.name" && v == "temper_write")
        );
    }

    #[test]
    fn split_span_hint_headers_strips_empty_values() {
        let headers = vec![
            ("X-Temper-Span-Name".to_string(), "".to_string()),
            (
                "X-Temper-Span-Attr-gen_ai.request.model".to_string(),
                "".to_string(),
            ),
            ("X-Temper-Span-Attr-".to_string(), "ignored".to_string()),
        ];
        let (kept, hints) = split_span_hint_headers(&headers);
        assert!(
            kept.is_empty(),
            "all x-temper-span-* headers should be stripped"
        );
        assert!(hints.span_name.is_none(), "empty name should be ignored");
        assert!(
            hints.attributes.is_empty(),
            "empty key or value should be ignored"
        );
    }

    #[test]
    fn split_span_hint_headers_strips_reserved_unknown_prefix() {
        // Future-proofing: unknown X-Temper-Span-* headers are stripped so they
        // don't leak to upstream services, but we don't act on them either.
        let headers = vec![
            ("X-Temper-Span-Future".to_string(), "whatever".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
        ];
        let (kept, hints) = split_span_hint_headers(&headers);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].0, "content-type");
        assert!(hints.span_name.is_none());
        assert!(hints.attributes.is_empty());
    }
}
