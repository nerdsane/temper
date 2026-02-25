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
            WasmAuthzDecision::Deny(reason) => Err(format!(
                "authorization denied for http_call to {domain}: {reason}"
            )),
        }
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_trait::SimWasmHost;

    /// A gate that denies everything.
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

    /// A gate that allows everything.
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
        WasmAuthzContext {
            tenant: "test-tenant".into(),
            module_name: "stripe_charge".into(),
            agent_id: Some("agent-1".into()),
            session_id: None,
            entity_type: "Order".into(),
            trigger_action: "submitOrder".into(),
        }
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
    fn logging_always_allowed() {
        let inner = Arc::new(SimWasmHost::new());
        let gate = Arc::new(DenyAllGate);
        let host = AuthorizedWasmHost::new(inner, gate, test_ctx());
        // Should not panic
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
    fn extract_domain_bare() {
        assert_eq!(extract_domain("https://example.com"), "example.com");
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
