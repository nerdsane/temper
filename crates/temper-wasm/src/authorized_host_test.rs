//! Authorization-wrapper tests, including ADR-0166 policy forwarding.
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
    fn authorize_secret_access(&self, _key: &str, _ctx: &WasmAuthzContext) -> WasmAuthzDecision {
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
    fn authorize_secret_access(&self, _key: &str, _ctx: &WasmAuthzContext) -> WasmAuthzDecision {
        WasmAuthzDecision::Allow
    }
}

fn test_ctx() -> WasmAuthzContext {
    WasmAuthzContext::test_fixture()
}

/// ADR-0166. This wrapper is what dispatch hands to the engine, so the
/// engine's view of the tenant's content decision is whatever this forwards.
/// Both directions matter: not forwarding `true` silently disables the opt-in
/// for every tenant, and not forwarding `false` would leak.
#[test]
fn authorized_host_forwards_the_llm_content_export_decision() {
    use crate::host_trait::ProductionWasmHost;
    use std::collections::BTreeMap;

    let opted_in: Arc<dyn WasmHost> =
        Arc::new(ProductionWasmHost::new(BTreeMap::new()).with_llm_content_export(true));
    let wrapped = AuthorizedWasmHost::new(opted_in, Arc::new(AllowAllGate), test_ctx());
    assert!(
        wrapped.exports_llm_content(),
        "an opted-in tenant's decision must survive the authz wrapper"
    );

    let redacted: Arc<dyn WasmHost> =
        Arc::new(ProductionWasmHost::new(BTreeMap::new()).with_llm_content_export(false));
    let wrapped = AuthorizedWasmHost::new(redacted, Arc::new(AllowAllGate), test_ctx());
    assert!(!wrapped.exports_llm_content());
}

/// A host that does not answer must redact. Any future `WasmHost` that forgets
/// to implement the method inherits this, so the default is the whole
/// protection for that host.
#[test]
fn wasm_host_defaults_to_redacting_llm_content() {
    struct MinimalHost;
    #[async_trait]
    impl WasmHost for MinimalHost {
        async fn http_call(
            &self,
            _method: &str,
            _url: &str,
            _headers: &[(String, String)],
            _body: &str,
        ) -> Result<(u16, String), String> {
            Ok((200, String::new()))
        }
        async fn http_call_binary(
            &self,
            _method: &str,
            _url: &str,
            _headers: &[(String, String)],
            _body: &[u8],
        ) -> Result<(u16, Vec<u8>), String> {
            Ok((200, Vec::new()))
        }
        fn get_secret(&self, _key: &str) -> Result<String, String> {
            Err("no secrets".to_string())
        }
        fn log(&self, _level: &str, _message: &str) {}
    }
    assert!(
        !MinimalHost.exports_llm_content(),
        "the WasmHost default must be redact; a host that does not opt in must \
         never export a tenant's LLM content"
    );
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
