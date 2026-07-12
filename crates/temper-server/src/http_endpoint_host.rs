//! Governed WASM host construction for HttpEndpoint (ADR-0158 / ARN-208).
//!
//! Ensures the inbound endpoint path uses the same authorization envelope as
//! entity WASM dispatch: bootstrap secrets only, gated secret resolver, and
//! `AuthorizedWasmHost` — never a raw production host with a full secret map.

use std::sync::Arc;

use temper_runtime::tenant::TenantId;
use temper_wasm::http_stream::HttpStreamRegistry;
use temper_wasm::types::{WasmAuthzContext, WasmInvocationContext};
use temper_wasm::{AuthorizedWasmHost, ProductionWasmHost, WasmHost};

use crate::state::ServerState;

/// Exact header names that must never be delivered to endpoint guests.
const STRIPPED_INBOUND_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "x-temper-api-key",
];

/// True when an inbound header must be stripped before guest delivery.
///
/// Exact denylist covers classic credential carriers. Prefix rules cover
/// ambient platform identity (`x-temper-principal*`, `x-temper-agent-*`)
/// so guests cannot inherit authority material from request headers
/// (ADR-0158 / ARN-208).
pub fn is_sensitive_inbound_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if STRIPPED_INBOUND_HEADERS.contains(&lower.as_str()) {
        return true;
    }
    lower.starts_with("x-temper-principal") || lower.starts_with("x-temper-agent-")
}

/// Filter inbound headers for guest `HttpDispatchContext` delivery.
pub fn guest_safe_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(k, _)| !is_sensitive_inbound_header(k))
        .cloned()
        .collect()
}

/// Build the governed host used for an HttpEndpoint WASM invocation.
pub fn build_httpendpoint_wasm_host(
    state: &ServerState,
    tenant: &TenantId,
    module_name: &str,
    streams: Arc<HttpStreamRegistry>,
    invocation_ctx: WasmInvocationContext,
) -> Arc<dyn WasmHost> {
    let gate = state.wasm_authz_gate();
    let authz_ctx = WasmAuthzContext {
        tenant: tenant.as_str().to_string(),
        module_name: module_name.to_string(),
        agent_id: None,
        session_id: None,
        entity_type: "HttpEndpoint".to_string(),
        trigger_action: "HandleHttp".to_string(),
    };

    // Bootstrap only — never clone the full tenant secret map (ARN-208).
    let bootstrap_secrets =
        state.get_authorized_wasm_host_bootstrap_secrets(tenant, &*gate, &authz_ctx);
    let secret_resolver =
        state.authorized_wasm_secret_resolver(tenant, Arc::clone(&gate), authz_ctx.clone());

    let mut production = ProductionWasmHost::with_shared_streams(bootstrap_secrets, streams)
        .with_invocation_context(invocation_ctx);
    if let Some(resolver) = secret_resolver {
        production = production.with_secret_resolver(resolver);
    }

    let inner: Arc<dyn WasmHost> = Arc::new(production);
    Arc::new(AuthorizedWasmHost::new(inner, gate, authz_ctx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::registry::SpecRegistry;
    use crate::secrets::SecretsVault;
    use temper_runtime::ActorSystem;
    use temper_wasm::types::WasmInvocationContext;

    fn deny_all_state_with_leaked_secret() -> ServerState {
        let vault = SecretsVault::new(&[9u8; 32]);
        vault
            .cache_secret(
                "default",
                "LEAKED_TENANT_SECRET",
                "should-not-be-eager".to_string(),
            )
            .expect("cache");
        let system = ActorSystem::new("httpendpoint-host-test");
        let state =
            ServerState::from_registry(system, SpecRegistry::new()).with_secrets_vault(vault);
        // Force empty Cedar policies so host ops are default-deny (ARN-208).
        state
            .authz
            .reload_policies("")
            .expect("empty policy set should parse");
        state
    }

    fn sample_invocation_ctx() -> WasmInvocationContext {
        WasmInvocationContext {
            tenant: "default".into(),
            entity_type: "HttpEndpoint".into(),
            entity_id: "ep-1".into(),
            trigger_action: "HandleHttp".into(),
            wasm_module: Some("mod".into()),
            trigger_params: serde_json::Value::Null,
            entity_state: serde_json::Value::Null,
            agent_id: None,
            session_id: None,
            integration_config: BTreeMap::new(),
            trace_id: String::new(),
            workflow_root_entity_type: None,
            workflow_root_entity_id: None,
            workflow_run_id: None,
            http_request: None,
        }
    }

    #[test]
    fn strips_authorization_and_principal_headers() {
        let headers = vec![
            ("content-type".into(), "application/json".into()),
            ("authorization".into(), "Bearer secret-token".into()),
            ("X-Temper-Principal-Kind".into(), "admin".into()),
            ("x-temper-principal-id".into(), "evil".into()),
            ("x-temper-principal-scopes".into(), "admin".into()),
            ("x-temper-agent-type".into(), "claude-code".into()),
            ("x-temper-agent-role".into(), "supervisor".into()),
            ("x-api-key".into(), "k".into()),
            ("accept".into(), "*/*".into()),
        ];
        let safe = guest_safe_headers(&headers);
        let names: Vec<_> = safe.iter().map(|(k, _)| k.to_ascii_lowercase()).collect();
        assert!(names.contains(&"content-type".to_string()));
        assert!(names.contains(&"accept".to_string()));
        assert!(!names.iter().any(|n| n.contains("authorization")));
        assert!(!names.iter().any(|n| n.contains("principal")));
        assert!(!names.iter().any(|n| n.contains("api-key")));
        assert!(!names.iter().any(|n| n.contains("agent-type")));
        assert!(!names.iter().any(|n| n.contains("agent-role")));
        assert!(!safe.iter().any(|(_, v)| v.contains("secret-token")));
        assert!(!safe.iter().any(|(_, v)| v == "admin" || v == "evil"));
    }

    #[test]
    fn sensitive_header_match_is_case_insensitive() {
        assert!(is_sensitive_inbound_header("Authorization"));
        assert!(is_sensitive_inbound_header("COOKIE"));
        assert!(is_sensitive_inbound_header("X-Temper-Agent-Type"));
        assert!(is_sensitive_inbound_header(
            "x-temper-principal-attr-region"
        ));
        assert!(!is_sensitive_inbound_header("content-type"));
        assert!(!is_sensitive_inbound_header("x-temper-observe-session-id"));
    }

    #[test]
    fn build_httpendpoint_host_does_not_require_full_secret_map() {
        let state = deny_all_state_with_leaked_secret();
        let tenant = TenantId::default();
        let streams = Arc::new(HttpStreamRegistry::new());
        let host =
            build_httpendpoint_wasm_host(&state, &tenant, "mod", streams, sample_invocation_ctx());
        // Host is constructed; secret lookups for non-bootstrap keys go through
        // the gated resolver and fail closed without Cedar permit.
        let err = host.get_secret("LEAKED_TENANT_SECRET");
        assert!(
            err.is_err(),
            "unauthorized secret must not be readable via endpoint host: {err:?}"
        );
        let msg = err.expect_err("denied");
        assert!(
            msg.contains("authorization denied"),
            "expected Cedar denial via AuthorizedWasmHost, got: {msg}"
        );
    }

    /// Proves the outer envelope is `AuthorizedWasmHost`: empty Cedar policies
    /// deny outbound HTTP. A bare `ProductionWasmHost` would attempt the call
    /// and fail with a network/client error instead of an authorization denial.
    #[tokio::test]
    async fn build_httpendpoint_host_gates_outbound_http() {
        let state = deny_all_state_with_leaked_secret();
        let tenant = TenantId::default();
        let streams = Arc::new(HttpStreamRegistry::new());
        let host =
            build_httpendpoint_wasm_host(&state, &tenant, "mod", streams, sample_invocation_ctx());
        let err = host
            .http_call("GET", "https://evil.example.com/ssrf", &[], "")
            .await
            .expect_err("outbound HTTP must be Cedar-gated");
        assert!(
            err.contains("authorization denied"),
            "expected AuthorizedWasmHost denial, got: {err}"
        );
    }
}
