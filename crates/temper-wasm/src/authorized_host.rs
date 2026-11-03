//! Authorization gate for WASM host functions.
//!
//! Provides a `WasmAuthzGate` trait for authorization decisions and an
//! `AuthorizedWasmHost` decorator that wraps any `WasmHost` and checks
//! authorization before delegating to the inner host.
//!
//! `temper-wasm` does NOT depend on `temper-authz`. The concrete Cedar
//! implementation (`CedarWasmAuthzGate`) lives in `temper-server`.

use std::sync::Arc;

use async_trait::async_trait;

use crate::host_trait::WasmHost;
use crate::types::WasmAuthzContext;

/// Authorization decision for a WASM host function call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WasmAuthzDecision {
    /// The call is allowed.
    Allow,
    /// The call is denied with a reason.
    Deny(String),
}

/// Trait for authorizing WASM host function calls.
///
/// Implemented by `CedarWasmAuthzGate` in `temper-server` for real Cedar
/// evaluation, and by `PermissiveWasmAuthzGate` for tests and ungated mode.
pub trait WasmAuthzGate: Send + Sync {
    /// Authorize an outbound HTTP call.
    ///
    /// - `domain`: extracted from the URL (e.g. "api.stripe.com")
    /// - `method`: HTTP method (e.g. "POST")
    /// - `url`: full URL
    /// - `ctx`: authorization context (tenant, module, agent, etc.)
    fn authorize_http_call(
        &self,
        domain: &str,
        method: &str,
        url: &str,
        ctx: &WasmAuthzContext,
    ) -> WasmAuthzDecision;

    /// Authorize access to a secret.
    ///
    /// - `secret_key`: the secret name (e.g. "STRIPE_API_KEY")
    /// - `ctx`: authorization context
    fn authorize_secret_access(
        &self,
        secret_key: &str,
        ctx: &WasmAuthzContext,
    ) -> WasmAuthzDecision;
}

/// The host an outbound request connects to, for the `http_call` gate.
///
/// Parses with `reqwest::Url`, the WHATWG parser the HTTP client uses, so the
/// gate decides on the host the request is sent to. Returns `None` for a URL
/// that does not parse, has no host, or carries userinfo: such a URL is
/// refused, never sent.
pub fn extract_domain(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return None;
    }
    parsed.host_str().map(str::to_owned)
}

/// Ask `gate` whether `operation` may send `method` to the host `url` connects
/// to. Guest calls come through here from [`AuthorizedWasmHost`]; so does every
/// redirect hop the production host follows (`host_trait/redirects.rs`).
pub(crate) fn authorize_outbound(
    gate: &dyn WasmAuthzGate,
    ctx: &WasmAuthzContext,
    operation: &str,
    method: &str,
    url: &str,
) -> Result<(), String> {
    let Some(domain) = extract_domain(url) else {
        tracing::warn!(
            tenant = %ctx.tenant,
            module = %ctx.module_name,
            entity_type = %ctx.entity_type,
            trigger_action = %ctx.trigger_action,
            operation,
            "WASM host refused an outbound URL with no host or with userinfo"
        );
        return Err(format!(
            "{operation} refused: the URL must name a host and carry no userinfo"
        ));
    };
    match gate.authorize_http_call(&domain, method, url, ctx) {
        WasmAuthzDecision::Allow => Ok(()),
        WasmAuthzDecision::Deny(reason) => {
            tracing::warn!(
                tenant = %ctx.tenant,
                module = %ctx.module_name,
                entity_type = %ctx.entity_type,
                trigger_action = %ctx.trigger_action,
                domain = %domain,
                http_method = %method,
                operation,
                reason = %reason,
                "WASM host authorization denied outbound HTTP call"
            );
            Err(format!(
                "authorization denied for {operation} to {domain}: {reason}"
            ))
        }
    }
}

/// Decorator that wraps a `WasmHost` and checks authorization before
/// delegating to the inner host.
///
/// If the gate denies the call, returns an error immediately without
/// calling the inner host.
pub struct AuthorizedWasmHost {
    /// The inner host to delegate to on Allow.
    inner: Arc<dyn WasmHost>,
    /// The authorization gate.
    gate: Arc<dyn WasmAuthzGate>,
    /// Authorization context for this invocation.
    ctx: WasmAuthzContext,
}

impl AuthorizedWasmHost {
    /// Create a new authorized host wrapping the given inner host.
    pub fn new(
        inner: Arc<dyn WasmHost>,
        gate: Arc<dyn WasmAuthzGate>,
        ctx: WasmAuthzContext,
    ) -> Self {
        Self { inner, gate, ctx }
    }

    fn authorize_http(&self, operation: &str, method: &str, url: &str) -> Result<(), String> {
        authorize_outbound(self.gate.as_ref(), &self.ctx, operation, method, url)
    }
}

#[async_trait]
impl WasmHost for AuthorizedWasmHost {
    /// Forward the wrapped host's per-tenant content decision (ADR-0166). This
    /// wrapper is what dispatch hands to the engine, so without forwarding the
    /// engine would read the trait default and redact even for a tenant that
    /// opted in — safe, but silently useless.
    fn exports_llm_content(&self) -> bool {
        self.inner.exports_llm_content()
    }

    async fn http_call(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<(u16, String), String> {
        self.authorize_http("http_call", method, url)?;
        self.inner.http_call(method, url, headers, body).await
    }

    async fn http_call_binary(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<(u16, Vec<u8>), String> {
        self.authorize_http("http_call_binary", method, url)?;
        self.inner
            .http_call_binary(method, url, headers, body)
            .await
    }

    async fn connect_call(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<Vec<String>, String> {
        self.authorize_http("connect_call", "POST", url)?;
        self.inner.connect_call(url, headers, body).await
    }

    async fn http_stream_begin_outbound(
        &self,
        request: crate::http_stream::HttpRequestHead,
    ) -> Result<crate::http_stream::HttpStreamHandles, String> {
        self.authorize_http("http_stream_begin_outbound", &request.method, &request.url)?;
        self.inner.http_stream_begin_outbound(request).await
    }

    async fn http_stream_read(
        &self,
        handle: crate::http_stream::StreamHandle,
    ) -> Result<Vec<u8>, crate::http_stream::StreamError> {
        self.inner.http_stream_read(handle).await
    }

    async fn http_stream_read_bounded(
        &self,
        handle: crate::http_stream::StreamHandle,
        max_bytes: usize,
    ) -> Result<Vec<u8>, crate::http_stream::StreamError> {
        self.inner.http_stream_read_bounded(handle, max_bytes).await
    }

    async fn http_stream_try_write(
        &self,
        handle: crate::http_stream::StreamHandle,
        chunk: Vec<u8>,
    ) -> Result<usize, crate::http_stream::StreamError> {
        self.inner.http_stream_try_write(handle, chunk).await
    }

    async fn http_stream_close(
        &self,
        handle: crate::http_stream::StreamHandle,
    ) -> Result<(), crate::http_stream::StreamError> {
        self.inner.http_stream_close(handle).await
    }

    async fn http_stream_response_head(
        &self,
        response_body: crate::http_stream::StreamHandle,
    ) -> Result<crate::http_stream::HttpResponseHead, String> {
        self.inner.http_stream_response_head(response_body).await
    }

    async fn http_stream_send_response_head(
        &self,
        response_body: crate::http_stream::StreamHandle,
        head: crate::http_stream::HttpResponseHead,
    ) -> Result<(), crate::http_stream::StreamError> {
        self.inner
            .http_stream_send_response_head(response_body, head)
            .await
    }

    fn get_secret(&self, key: &str) -> Result<String, String> {
        match self.gate.authorize_secret_access(key, &self.ctx) {
            WasmAuthzDecision::Allow => self.inner.get_secret(key),
            WasmAuthzDecision::Deny(reason) => {
                Err(format!("authorization denied for secret '{key}': {reason}"))
            }
        }
    }

    fn log(&self, level: &str, message: &str) {
        // Logging is always allowed — no authorization check needed.
        self.inner.log(level, message);
    }

    fn evaluate_spec(
        &self,
        ioa_source: &str,
        current_state: &str,
        action: &str,
        params_json: &str,
    ) -> Result<String, String> {
        // Spec evaluation is a local computation — no authorization needed.
        self.inner
            .evaluate_spec(ioa_source, current_state, action, params_json)
    }

    fn emit_progress(&self, event_json: &str) -> Result<(), String> {
        self.inner.emit_progress(event_json)
    }

    fn emit_wide_event(&self, event_json: &str) -> Result<(), String> {
        self.inner.emit_wide_event(event_json)
    }

    fn log_structured(&self, log_json: &str) -> Result<(), String> {
        self.inner.log_structured(log_json)
    }

    fn emit_metric(&self, metric_json: &str) -> Result<(), String> {
        self.inner.emit_metric(metric_json)
    }
}

#[cfg(test)]
#[path = "authorized_host_test.rs"]
mod tests;
