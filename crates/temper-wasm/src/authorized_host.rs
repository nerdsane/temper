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

/// Extract domain from a URL using simple string parsing.
///
/// Finds `://`, strips any `user:pass@` userinfo, then takes everything
/// up to the next `/`, `?`, or `:` (port). Returns the domain or the
/// full URL if parsing fails.
pub fn extract_domain(url: &str) -> &str {
    let after_scheme = url.find("://").map(|i| &url[i + 3..]).unwrap_or(url);
    // Strip userinfo if present (user:pass@host) to prevent SSRF bypass
    let after_auth = after_scheme
        .find('@')
        .map(|i| &after_scheme[i + 1..])
        .unwrap_or(after_scheme);
    // Take up to the first '/', '?', or ':' (port separator)
    let end = after_auth.find(['/', '?', ':']).unwrap_or(after_auth.len());
    &after_auth[..end]
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
}

#[async_trait]
impl WasmHost for AuthorizedWasmHost {
    fn temper_data_request_budget(&self) -> usize {
        self.inner.temper_data_request_budget()
    }
    fn temper_data_response_handle_budget(&self) -> usize {
        self.inner.temper_data_response_handle_budget()
    }
    async fn temper_data_call(&self, request: &[u8]) -> Result<Vec<u8>, String> {
        self.inner.temper_data_call(request).await
    }
    fn temper_file_stream_read(&self, handle: u32, max_bytes: usize) -> Result<Vec<u8>, i32> {
        self.inner.temper_file_stream_read(handle, max_bytes)
    }
    fn temper_file_stream_try_write(&self, handle: u32, bytes: &[u8]) -> Result<usize, i32> {
        self.inner.temper_file_stream_try_write(handle, bytes)
    }

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
        let domain = extract_domain(url);
        match self
            .gate
            .authorize_http_call(domain, method, url, &self.ctx)
        {
            WasmAuthzDecision::Allow => self.inner.http_call(method, url, headers, body).await,
            WasmAuthzDecision::Deny(reason) => {
                tracing::warn!(
                    tenant = %self.ctx.tenant,
                    module = %self.ctx.module_name,
                    entity_type = %self.ctx.entity_type,
                    trigger_action = %self.ctx.trigger_action,
                    domain = %domain,
                    http_method = %method,
                    reason = %reason,
                    "WASM host authorization denied outbound HTTP call"
                );
                Err(format!(
                    "authorization denied for http_call to {domain}: {reason}"
                ))
            }
        }
    }

    async fn http_call_binary(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<(u16, Vec<u8>), String> {
        let domain = extract_domain(url);
        match self
            .gate
            .authorize_http_call(domain, method, url, &self.ctx)
        {
            WasmAuthzDecision::Allow => {
                self.inner
                    .http_call_binary(method, url, headers, body)
                    .await
            }
            WasmAuthzDecision::Deny(reason) => {
                tracing::warn!(
                    tenant = %self.ctx.tenant,
                    module = %self.ctx.module_name,
                    entity_type = %self.ctx.entity_type,
                    trigger_action = %self.ctx.trigger_action,
                    domain = %domain,
                    http_method = %method,
                    reason = %reason,
                    "WASM host authorization denied outbound binary HTTP call"
                );
                Err(format!(
                    "authorization denied for http_call_binary to {domain}: {reason}"
                ))
            }
        }
    }

    async fn connect_call(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<Vec<String>, String> {
        let domain = extract_domain(url);
        match self
            .gate
            .authorize_http_call(domain, "POST", url, &self.ctx)
        {
            WasmAuthzDecision::Allow => self.inner.connect_call(url, headers, body).await,
            WasmAuthzDecision::Deny(reason) => {
                tracing::warn!(
                    tenant = %self.ctx.tenant,
                    module = %self.ctx.module_name,
                    entity_type = %self.ctx.entity_type,
                    trigger_action = %self.ctx.trigger_action,
                    domain = %domain,
                    reason = %reason,
                    "WASM host authorization denied Connect call"
                );
                Err(format!(
                    "authorization denied for connect_call to {domain}: {reason}"
                ))
            }
        }
    }

    async fn http_stream_begin_outbound(
        &self,
        request: crate::http_stream::HttpRequestHead,
    ) -> Result<crate::http_stream::HttpStreamHandles, String> {
        let domain = extract_domain(&request.url);
        match self
            .gate
            .authorize_http_call(domain, &request.method, &request.url, &self.ctx)
        {
            WasmAuthzDecision::Allow => self.inner.http_stream_begin_outbound(request).await,
            WasmAuthzDecision::Deny(reason) => {
                tracing::warn!(
                    tenant = %self.ctx.tenant,
                    module = %self.ctx.module_name,
                    entity_type = %self.ctx.entity_type,
                    trigger_action = %self.ctx.trigger_action,
                    domain = %domain,
                    http_method = %request.method,
                    reason = %reason,
                    "WASM host authorization denied outbound streaming HTTP call"
                );
                Err(format!(
                    "authorization denied for http_stream_begin_outbound to {domain}: {reason}"
                ))
            }
        }
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
mod tests {
    use super::*;
    use crate::host_trait::SimWasmHost;

    struct DenyAllGate;
    impl WasmAuthzGate for DenyAllGate {
        fn authorize_http_call(
            &self,
            _domain: &str,
            _method: &str,
            _url: &str,
            _ctx: &WasmAuthzContext,
        ) -> WasmAuthzDecision {
            WasmAuthzDecision::Deny("denied by policy".into())
        }
        fn authorize_secret_access(
            &self,
            _key: &str,
            _ctx: &WasmAuthzContext,
        ) -> WasmAuthzDecision {
            WasmAuthzDecision::Deny("denied by policy".into())
        }
    }

    struct AllowAllGate;
    impl WasmAuthzGate for AllowAllGate {
        fn authorize_http_call(
            &self,
            _domain: &str,
            _method: &str,
            _url: &str,
            _ctx: &WasmAuthzContext,
        ) -> WasmAuthzDecision {
            WasmAuthzDecision::Allow
        }
        fn authorize_secret_access(
            &self,
            _key: &str,
            _ctx: &WasmAuthzContext,
        ) -> WasmAuthzDecision {
            WasmAuthzDecision::Allow
        }
    }

    fn test_ctx() -> WasmAuthzContext {
        WasmAuthzContext::test_fixture()
    }

    #[tokio::test]
    async fn deny_gate_blocks_http_call() {
        let inner = Arc::new(SimWasmHost::new());
        let gate = Arc::new(DenyAllGate);
        let host = AuthorizedWasmHost::new(inner, gate, test_ctx());

        let result = host
            .http_call("POST", "https://api.stripe.com/v1/charges", &[], "")
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("authorization denied"));
    }

    #[tokio::test]
    async fn deny_gate_blocks_secret_access() {
        let inner = Arc::new(SimWasmHost::new().with_secret("STRIPE_API_KEY", "sk-test"));
        let gate = Arc::new(DenyAllGate);
        let host = AuthorizedWasmHost::new(inner, gate, test_ctx());

        let result = host.get_secret("STRIPE_API_KEY");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("authorization denied"));
    }

    #[tokio::test]
    async fn allow_gate_delegates_http_call() {
        let inner = Arc::new(SimWasmHost::new());
        let gate = Arc::new(AllowAllGate);
        let host = AuthorizedWasmHost::new(inner, gate, test_ctx());

        let result = host
            .http_call("GET", "https://api.stripe.com/v1/charges", &[], "")
            .await;
        assert!(result.is_ok());
        let (status, _body) = result.unwrap();
        assert_eq!(status, 200);
    }

    #[tokio::test]
    async fn allow_gate_delegates_secret_access() {
        let inner = Arc::new(SimWasmHost::new().with_secret("KEY", "val"));
        let gate = Arc::new(AllowAllGate);
        let host = AuthorizedWasmHost::new(inner, gate, test_ctx());

        let result = host.get_secret("KEY");
        assert_eq!(result, Ok("val".into()));
    }

    #[test]
    fn allow_gate_delegates_evaluate_spec() {
        let ioa_source = "[automaton]\nname = \"Issue\"";
        let ioa_hash = format!("{:x}", ioa_source.len());
        let inner = Arc::new(SimWasmHost::new().with_spec_eval_response(
            &ioa_hash,
            "Reassign",
            r#"{"success":true,"new_state":"InProgress"}"#,
        ));
        let gate = Arc::new(AllowAllGate);
        let host = AuthorizedWasmHost::new(inner, gate, test_ctx());

        let result = host.evaluate_spec(ioa_source, "Backlog", "Reassign", "{}");
        assert!(
            result.is_ok(),
            "evaluate_spec should delegate to inner host"
        );
        assert!(
            result.unwrap_or_default().contains(r#""success":true"#),
            "expected canned evaluate_spec response from inner host"
        );
    }

    #[test]
    fn logging_always_allowed() {
        let inner = Arc::new(SimWasmHost::new());
        let gate = Arc::new(DenyAllGate);
        let host = AuthorizedWasmHost::new(inner, gate, test_ctx());
        host.log("info", "test message");
    }

    #[test]
    fn extract_domain_https() {
        assert_eq!(
            extract_domain("https://api.stripe.com/v1/charges"),
            "api.stripe.com"
        );
    }

    #[test]
    fn extract_domain_http() {
        assert_eq!(extract_domain("http://localhost:8080/api"), "localhost");
    }

    #[test]
    fn extract_domain_with_port() {
        assert_eq!(
            extract_domain("https://example.com:443/path"),
            "example.com"
        );
    }

    #[test]
    fn extract_domain_no_scheme() {
        assert_eq!(extract_domain("api.stripe.com/path"), "api.stripe.com");
    }

    #[test]
    fn extract_domain_ip() {
        assert_eq!(extract_domain("http://127.0.0.1:3000/api"), "127.0.0.1");
    }

    #[test]
    fn extract_domain_strips_userinfo() {
        assert_eq!(
            extract_domain("https://attacker:pass@localhost/exploit"),
            "localhost"
        );
    }
}

#[cfg(test)]
#[path = "authorized_host_data_test.rs"]
mod data_test;

#[cfg(test)]
#[path = "authorized_host_test.rs"]
mod security_tests;
