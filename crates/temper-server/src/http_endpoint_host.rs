//! Governed WASM host construction for HttpEndpoint (ADR-0158 / ARN-208).
//!
//! Ensures the inbound endpoint path uses the same authorization envelope as
//! entity WASM dispatch: bootstrap secrets only, gated secret resolver, and
//! `AuthorizedWasmHost` — never a raw production host with a full secret map.

use std::collections::BTreeMap;
use std::sync::Arc;

use temper_runtime::tenant::TenantId;
use temper_wasm::http_stream::HttpStreamRegistry;
use temper_wasm::types::{WasmAuthzContext, WasmInvocationContext};
use temper_wasm::{AuthorizedWasmHost, ProductionWasmHost, WasmHost};

use crate::state::ServerState;

/// Header names that must never be delivered to endpoint guests.
const STRIPPED_INBOUND_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "x-temper-api-key",
    "x-temper-principal-id",
    "x-temper-principal-kind",
    "x-temper-principal-scopes",
];

/// True when an inbound header must be stripped before guest delivery.
pub fn is_sensitive_inbound_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    STRIPPED_INBOUND_HEADERS.contains(&lower.as_str())
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

/// Empty secret map proof helper for tests and diagnostics.
pub fn empty_secret_map() -> BTreeMap<String, String> {
    BTreeMap::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_authorization_and_principal_headers() {
        let headers = vec![
            ("content-type".into(), "application/json".into()),
            ("authorization".into(), "Bearer secret-token".into()),
            ("X-Temper-Principal-Kind".into(), "admin".into()),
            ("x-temper-principal-id".into(), "evil".into()),
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
        assert!(!safe.iter().any(|(_, v)| v.contains("secret-token")));
    }

    #[test]
    fn sensitive_header_match_is_case_insensitive() {
        assert!(is_sensitive_inbound_header("Authorization"));
        assert!(is_sensitive_inbound_header("COOKIE"));
        assert!(!is_sensitive_inbound_header("content-type"));
    }

    #[test]
    fn build_httpendpoint_host_does_not_require_full_secret_map() {
        use crate::registry::SpecRegistry;
        use crate::secrets::SecretsVault;
        use temper_runtime::ActorSystem;
        use temper_wasm::types::WasmInvocationContext;

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
        // Default ServerState uses a permissive authz engine for local/dev.
        // Force empty Cedar policies so access_secret is default-deny (ARN-208).
        state
            .authz
            .reload_policies("")
            .expect("empty policy set should parse");
        let tenant = TenantId::default();
        let streams = Arc::new(HttpStreamRegistry::new());
        let inv = WasmInvocationContext {
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
        };
        let host = build_httpendpoint_wasm_host(&state, &tenant, "mod", streams, inv);
        // Host is constructed; secret lookups for non-bootstrap keys go through
        // the gated resolver and fail closed without Cedar permit.
        let err = host.get_secret("LEAKED_TENANT_SECRET");
        assert!(
            err.is_err(),
            "unauthorized secret must not be readable via endpoint host: {err:?}"
        );
    }
}
