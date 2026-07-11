use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use temper_runtime::ActorSystem;
use temper_spec::csdl::CsdlDocument;
use temper_wasm::WasmHost;

use super::*;

struct HeaderCaptureHost {
    headers: Arc<Mutex<Vec<(String, String)>>>,
}

#[async_trait]
impl WasmHost for HeaderCaptureHost {
    async fn http_call(
        &self,
        _method: &str,
        _url: &str,
        headers: &[(String, String)],
        _body: &str,
    ) -> Result<(u16, String), String> {
        *self.headers.lock().expect("header capture lock") = headers.to_vec();
        Ok((200, "{}".to_string()))
    }

    fn get_secret(&self, key: &str) -> Result<String, String> {
        Err(format!("secret not found: {key}"))
    }

    async fn http_call_binary(
        &self,
        _method: &str,
        _url: &str,
        headers: &[(String, String)],
        _body: &[u8],
    ) -> Result<(u16, Vec<u8>), String> {
        *self.headers.lock().expect("header capture lock") = headers.to_vec();
        Ok((200, Vec::new()))
    }

    async fn connect_call(
        &self,
        _url: &str,
        headers: &[(String, String)],
        _body: &str,
    ) -> Result<Vec<String>, String> {
        *self.headers.lock().expect("header capture lock") = headers.to_vec();
        Ok(Vec::new())
    }

    async fn http_stream_begin_outbound(
        &self,
        request: HttpRequestHead,
    ) -> Result<HttpStreamHandles, String> {
        *self.headers.lock().expect("header capture lock") = request.headers;
        Ok(HttpStreamHandles {
            request_body: StreamHandle(1),
            response_body: StreamHandle(2),
        })
    }

    fn log(&self, _level: &str, _message: &str) {}
}

fn state() -> ServerState {
    let mut state = ServerState::new(
        ActorSystem::new("callback-header-test"),
        CsdlDocument {
            version: "4.0".to_string(),
            schemas: Vec::new(),
        },
        String::new(),
    )
    .with_secrets_vault(crate::secrets::vault::SecretsVault::new(&[7; 32]));
    state.local_tdata_hosts = Arc::new(BTreeSet::from(["temper.example".to_string()]));
    state
}

fn registration_body(entity_id: &str) -> String {
    serde_json::json!({
        "callback_tenant": "default",
        "callback_entity_set": "Sessions",
        "callback_entity_id": entity_id,
        "callback_on_approve": "Resume",
        "callback_on_deny": "Fail",
    })
    .to_string()
}

#[tokio::test]
async fn outbound_registration_is_signed_by_exact_invoking_target() {
    let state = state();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let host = LocalTDataWasmHost::new_for_invocation(
        state.clone(),
        TenantId::new("default"),
        "Session",
        "session-1",
        None,
        Arc::new(HeaderCaptureHost {
            headers: captured.clone(),
        }),
    );
    let headers = host
        .callback_registration_headers(
            "POST",
            "https://temper.example/tdata/GovernanceDecisions('gd-1')/Temper.System.RegisterCallback",
            &[],
            &registration_body("session-1"),
        )
        .expect("mint trusted registration")
        .expect("recognized trusted registration");
    assert!(
        captured.lock().expect("header capture lock").is_empty(),
        "callback authority must be consumed by local ingress, never delegated"
    );
    let encoded = headers
        .iter()
        .find(|(name, _)| name == "x-temper-callback-capability")
        .map(|(_, value)| value)
        .expect("signed capability header");
    let capability = state
        .verify_governance_callback_capability(encoded)
        .expect("verify signed header");
    assert_eq!(capability.source_governance_decision_id, "gd-1");
    assert_eq!(capability.target_tenant, "default");
    assert_eq!(capability.target_entity_type, "Session");
    assert_eq!(capability.target_entity_id, "session-1");
    assert_eq!(capability.approve_action, "Resume");
    assert_eq!(capability.deny_action, "Fail");
}

#[tokio::test]
async fn outbound_registration_cannot_target_another_entity() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let host = LocalTDataWasmHost::new_for_invocation(
        state(),
        TenantId::new("default"),
        "Session",
        "session-1",
        None,
        Arc::new(HeaderCaptureHost {
            headers: captured.clone(),
        }),
    );
    let error = host
        .http_call(
            "POST",
            "https://temper.example/tdata/GovernanceDecisions('gd-1')/RegisterCallback",
            &[],
            &registration_body("session-2"),
        )
        .await
        .expect_err("cross-entity callback mint must fail");
    assert!(error.contains("exact invoking tenant/entity"));
    assert!(captured.lock().expect("header capture lock").is_empty());
}

#[tokio::test]
async fn external_lookalike_registration_receives_no_capability() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let host = LocalTDataWasmHost::new_for_invocation(
        state(),
        TenantId::new("default"),
        "Session",
        "session-1",
        None,
        Arc::new(HeaderCaptureHost {
            headers: captured.clone(),
        }),
    );
    host.http_call(
        "POST",
        "https://attacker.example/tdata/GovernanceDecisions('gd-1')/RegisterCallback",
        &[
            ("authorization".to_string(), "attacker-visible".to_string()),
            ("x-tenant-id".to_string(), "temper-system".to_string()),
            ("x-temper-principal-kind".to_string(), "admin".to_string()),
        ],
        &registration_body("session-1"),
    )
    .await
    .expect("ordinary external delegation");
    let headers = captured.lock().expect("header capture lock");
    assert!(
        headers
            .iter()
            .all(|(name, _)| name != "x-temper-callback-capability")
    );
    assert!(
        headers
            .iter()
            .all(|(name, _)| !is_temper_trust_header(name))
    );
}

#[tokio::test]
async fn every_external_transport_strips_guest_supplied_trust_headers() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let host = LocalTDataWasmHost::new_for_invocation(
        state(),
        TenantId::new("default"),
        "Session",
        "session-1",
        None,
        Arc::new(HeaderCaptureHost {
            headers: captured.clone(),
        }),
    );
    let headers = vec![
        ("authorization".to_string(), "opaque".to_string()),
        ("x-tenant-id".to_string(), "temper-system".to_string()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
        ("x-temper-agent-role".to_string(), "operator".to_string()),
    ];
    let assert_sanitized = || {
        let captured = captured.lock().expect("header capture lock");
        assert_eq!(
            captured.as_slice(),
            &[("authorization".to_string(), "opaque".to_string())]
        );
    };

    host.http_call_binary("POST", "https://external.example/upload", &headers, b"body")
        .await
        .expect("binary delegation");
    assert_sanitized();
    host.connect_call("https://external.example/connect", &headers, "body")
        .await
        .expect("connect delegation");
    assert_sanitized();
    host.http_stream_begin_outbound(HttpRequestHead {
        method: "POST".to_string(),
        url: "https://external.example/stream".to_string(),
        headers,
    })
    .await
    .expect("stream delegation");
    assert_sanitized();
}

#[test]
fn local_header_map_cannot_override_invoking_tenant_or_principal() {
    let inherited = SecurityContext::from_headers(&[
        (
            "x-temper-principal-id".to_string(),
            "customer-1".to_string(),
        ),
        (
            "x-temper-principal-kind".to_string(),
            "customer".to_string(),
        ),
    ]);
    let map = header_map(
        &[
            ("x-tenant-id".to_string(), "temper-system".to_string()),
            ("x-temper-principal-kind".to_string(), "admin".to_string()),
            ("x-temper-principal-id".to_string(), "attacker".to_string()),
        ],
        &TenantId::new("tenant-a"),
        &security_context_headers(&inherited),
    );
    assert_eq!(
        map.get("x-tenant-id").and_then(|value| value.to_str().ok()),
        Some("tenant-a")
    );
    assert_eq!(
        map.get("x-temper-principal-kind")
            .and_then(|value| value.to_str().ok()),
        Some("customer")
    );
    assert_eq!(
        map.get("x-temper-principal-id")
            .and_then(|value| value.to_str().ok()),
        Some("customer-1")
    );
}
