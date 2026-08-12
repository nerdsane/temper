use super::*;
use axum::http::StatusCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use temper_runtime::ActorSystem;
use temper_spec::csdl::parse_csdl;

const ORDER_CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Local" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Order">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Customer" Type="Edm.String"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <Action Name="SubmitOrder" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.Local.Order"/>
      </Action>
      <EntityContainer Name="Container">
        <EntitySet Name="Orders" EntityType="Temper.Local.Order"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const ORDER_IOA: &str = r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted"]
initial = "Draft"

[[action]]
name = "SubmitOrder"
kind = "input"
from = ["Draft"]
to = "Submitted"
"#;

struct FailingHost;

#[async_trait]
impl WasmHost for FailingHost {
    async fn http_call(
        &self,
        _method: &str,
        _url: &str,
        _headers: &[(String, String)],
        _body: &str,
    ) -> Result<(u16, String), String> {
        Err("delegate should not receive local TData calls".to_string())
    }

    fn get_secret(&self, key: &str) -> Result<String, String> {
        Err(format!("secret not found: {key}"))
    }

    async fn http_call_binary(
        &self,
        _method: &str,
        _url: &str,
        _headers: &[(String, String)],
        _body: &[u8],
    ) -> Result<(u16, Vec<u8>), String> {
        Err("binary delegate not used".to_string())
    }

    fn log(&self, _level: &str, _message: &str) {}
}

struct CountingHost {
    calls: Arc<AtomicUsize>,
    stream_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl WasmHost for CountingHost {
    async fn http_call(
        &self,
        _method: &str,
        _url: &str,
        _headers: &[(String, String)],
        _body: &str,
    ) -> Result<(u16, String), String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok((299, "delegated".to_string()))
    }

    fn get_secret(&self, key: &str) -> Result<String, String> {
        Err(format!("secret not found: {key}"))
    }

    async fn http_call_binary(
        &self,
        _method: &str,
        _url: &str,
        _headers: &[(String, String)],
        _body: &[u8],
    ) -> Result<(u16, Vec<u8>), String> {
        Ok((299, b"delegated-binary".to_vec()))
    }

    fn log(&self, _level: &str, _message: &str) {}

    async fn http_stream_begin_outbound(
        &self,
        _request: HttpRequestHead,
    ) -> Result<HttpStreamHandles, String> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        Ok(HttpStreamHandles {
            request_body: StreamHandle(11),
            response_body: StreamHandle(12),
        })
    }

    async fn http_stream_read(&self, _handle: StreamHandle) -> Result<Vec<u8>, StreamError> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        Ok(b"delegated-direct-read".to_vec())
    }

    async fn http_stream_read_bounded(
        &self,
        _handle: StreamHandle,
        _max_bytes: usize,
    ) -> Result<Vec<u8>, StreamError> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        Ok(b"delegated-bounded-read".to_vec())
    }

    async fn http_stream_try_write(
        &self,
        _handle: StreamHandle,
        chunk: Vec<u8>,
    ) -> Result<usize, StreamError> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        Ok(chunk.len())
    }

    async fn http_stream_close(&self, _handle: StreamHandle) -> Result<(), StreamError> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn http_stream_response_head(
        &self,
        _response_body: StreamHandle,
    ) -> Result<HttpResponseHead, String> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        Ok(HttpResponseHead {
            status: 299,
            headers: vec![("x-test-stream".to_string(), "delegated".to_string())],
        })
    }

    async fn http_stream_send_response_head(
        &self,
        _response_body: StreamHandle,
        _head: HttpResponseHead,
    ) -> Result<(), StreamError> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn test_state() -> ServerState {
    let csdl = parse_csdl(ORDER_CSDL_XML).expect("test CSDL should parse");
    let system = ActorSystem::new("local-tdata-wasm-host-test");
    let mut specs = BTreeMap::new();
    specs.insert("Order".to_string(), ORDER_IOA.to_string());
    ServerState::with_specs(system, csdl, ORDER_CSDL_XML.to_string(), specs)
        .expect("test state should build")
}

fn customer_security_context(id: &str) -> SecurityContext {
    SecurityContext {
        principal: temper_authz::Principal {
            id: id.to_string(),
            kind: temper_authz::PrincipalKind::Customer,
            role: None,
            acting_for: None,
            agent_type: None,
            attributes: Default::default(),
        },
        context_attrs: Default::default(),
        correlation_id: "local-tdata-test".to_string(),
    }
}

#[test]
fn parses_loopback_tdata_request() {
    let request = LocalTDataRequest::parse(
        "http://127.0.0.1:8787/tdata/SessionEntries?$filter=SessionId%20eq%20%27s1%27&$top=1",
        &BTreeSet::new(),
    )
    .expect("loopback TData URL should parse");

    assert_eq!(request.path, "SessionEntries");
    assert_eq!(
        request.query.get("$filter").map(String::as_str),
        Some("SessionId eq 's1'")
    );
    assert_eq!(request.query.get("$top").map(String::as_str), Some("1"));
}

#[test]
fn ignores_non_tdata_or_non_loopback_urls() {
    assert!(
        LocalTDataRequest::parse("https://api.example.com/tdata/Orders", &BTreeSet::new())
            .is_none()
    );
    assert!(
        LocalTDataRequest::parse("http://127.0.0.1:8787/api/health", &BTreeSet::new()).is_none()
    );
    assert!(LocalTDataRequest::parse("not a url", &BTreeSet::new()).is_none());
}

#[test]
fn parses_allowlisted_public_tdata_request() {
    let local_hosts = BTreeSet::from(["temper.example".to_string()]);
    let request =
        LocalTDataRequest::parse("https://TEMPER.example/tdata/Orders?$top=1", &local_hosts)
            .expect("allowlisted public TData URL should parse");

    assert_eq!(request.path, "Orders");
    assert_eq!(request.query.get("$top").map(String::as_str), Some("1"));
}

#[test]
fn local_tdata_headers_discard_guest_authority_and_tenant() {
    let map = header_map(&[
        ("accept".to_string(), "application/json".to_string()),
        ("x-tenant-id".to_string(), "victim".to_string()),
        ("x-temper-principal-id".to_string(), "attacker".to_string()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
        ("x-temper-agent-role".to_string(), "supervisor".to_string()),
        ("x-temper-principal-scopes".to_string(), "root".to_string()),
        ("x-temper-attr-region".to_string(), "all".to_string()),
        ("x-temper-action-context".to_string(), "forged".to_string()),
        (
            "x-temper-workflow-run-id".to_string(),
            "workflow-1".to_string(),
        ),
    ]);

    assert!(map.get("x-tenant-id").is_none());
    assert!(map.get("x-temper-principal-id").is_none());
    assert!(map.get("x-temper-principal-kind").is_none());
    assert!(map.get("x-temper-agent-role").is_none());
    assert!(map.get("x-temper-principal-scopes").is_none());
    assert!(map.get("x-temper-attr-region").is_none());
    assert!(map.get("x-temper-action-context").is_none());
    assert_eq!(
        map.get("x-temper-workflow-run-id")
            .and_then(|value| value.to_str().ok()),
        Some("workflow-1")
    );
}

#[tokio::test]
async fn local_tdata_calls_use_odata_handlers() {
    let host = LocalTDataWasmHost::new(
        test_state(),
        temper_runtime::tenant::TenantId::default(),
        Some(&SecurityContext::system()),
        Arc::new(FailingHost),
    );
    let headers = vec![
        ("x-tenant-id".to_string(), "default".to_string()),
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), "application/json".to_string()),
    ];

    let (status, body) = host
        .http_call(
            "POST",
            "http://127.0.0.1:8787/tdata/Orders",
            &headers,
            r#"{"id":"order-local-1","Customer":"Ada"}"#,
        )
        .await
        .expect("local create should succeed");
    assert_eq!(status, StatusCode::CREATED.as_u16());
    let created: serde_json::Value = serde_json::from_str(&body).expect("created JSON");
    assert_eq!(created["entity_id"], "order-local-1");

    let (status, body) = host
        .http_call(
            "GET",
            "http://localhost:8787/tdata/Orders('order-local-1')",
            &headers,
            "",
        )
        .await
        .expect("local read should succeed");
    assert_eq!(status, StatusCode::OK.as_u16());
    let fetched: serde_json::Value = serde_json::from_str(&body).expect("fetched JSON");
    assert_eq!(fetched["fields"]["Customer"], "Ada");

    let (status, body) = host
        .http_call(
            "POST",
            "http://[::1]:8787/tdata/Orders('order-local-1')/Temper.Local.SubmitOrder",
            &headers,
            "{}",
        )
        .await
        .expect("local action should succeed");
    assert_eq!(status, StatusCode::OK.as_u16());
    let submitted: serde_json::Value = serde_json::from_str(&body).expect("action JSON");
    assert_eq!(submitted["status"], "Submitted");
}

/// ARN-170 regression guard for the direct-invocation (blob_adapter) loopback.
///
/// This drives the real production helper `ServerState::local_tdata_direct_host`
/// that `invoke_wasm_direct` uses, so it guards the actual authority decision (not
/// just the `LocalTDataWasmHost` contract): the helper must build the loopback
/// WITH server-minted authority. The delegate is `FailingHost`, so if the helper
/// regresses to no authority the `/tdata` call falls through to it and the test
/// fails — exactly the silent-401 blob regression ARN-170 introduced and this
/// fix closes.
#[tokio::test]
async fn direct_invocation_loopback_dispatches_in_process_with_system_authority() {
    let state = test_state();
    let host = state.local_tdata_direct_host(&TenantId::default(), Arc::new(FailingHost));
    let headers = vec![
        ("x-tenant-id".to_string(), "default".to_string()),
        ("accept".to_string(), "application/json".to_string()),
    ];

    let (status, _body) = host
        .http_call("GET", "http://127.0.0.1:8787/tdata/Orders", &headers, "")
        .await
        .expect("direct-invocation loopback must dispatch in-process, not delegate");
    assert_eq!(status, StatusCode::OK.as_u16());
}

#[tokio::test]
async fn local_tdata_forged_admin_headers_cannot_upgrade_customer() {
    let state = test_state();
    state
        .authz
        .reload_tenant_policies(
            TenantId::default().as_str(),
            "permit(principal is Admin, action, resource);",
        )
        .expect("admin-only policy should parse");
    let customer = customer_security_context("customer-1");
    let host = LocalTDataWasmHost::new(
        state,
        TenantId::default(),
        Some(&customer),
        Arc::new(FailingHost),
    );
    let headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
        ("x-temper-principal-id".to_string(), "attacker".to_string()),
        ("x-temper-principal-scopes".to_string(), "root".to_string()),
        ("x-temper-attr-owner".to_string(), "*".to_string()),
        ("x-temper-action-context".to_string(), "forged".to_string()),
    ];

    let (status, body) = host
        .http_call(
            "POST",
            "http://127.0.0.1:8787/tdata/Orders",
            &headers,
            r#"{"id":"forged-admin-order"}"#,
        )
        .await
        .expect("local OData response should be returned");

    assert_eq!(status, StatusCode::FORBIDDEN.as_u16(), "{body}");
}

#[tokio::test]
async fn local_tdata_uses_exact_agent_and_ignores_guest_tenant() {
    let state = test_state();
    state
        .authz
        .reload_tenant_policies(
            TenantId::default().as_str(),
            "permit(principal is Agent, action, resource);",
        )
        .expect("agent-only policy should parse");
    let agent = SecurityContext::from_resolved_identity("agent-1", "operator", None);
    let host = LocalTDataWasmHost::new(
        state.clone(),
        TenantId::default(),
        Some(&agent),
        Arc::new(FailingHost),
    );
    let headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("x-tenant-id".to_string(), "victim".to_string()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
        ("x-temper-principal-id".to_string(), "attacker".to_string()),
    ];

    let (status, body) = host
        .http_call(
            "POST",
            "http://localhost:8787/tdata/Orders",
            &headers,
            r#"{"id":"exact-agent-order"}"#,
        )
        .await
        .expect("local OData response should be returned");

    assert_eq!(status, StatusCode::CREATED.as_u16(), "{body}");
    assert!(state.entity_exists(&TenantId::default(), "Order", "exact-agent-order"));
    assert!(!state.entity_exists(&TenantId::new("victim"), "Order", "exact-agent-order"));
}

#[tokio::test]
async fn local_tdata_uses_invocation_tenant_without_a_tenant_header() {
    let mut state = test_state();
    state.single_tenant_mode = false;
    let host = LocalTDataWasmHost::new(
        state,
        temper_runtime::tenant::TenantId::default(),
        Some(&SecurityContext::system()),
        Arc::new(FailingHost),
    );
    let headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), "application/json".to_string()),
    ];

    let (status, body) = host
        .http_call(
            "POST",
            "http://127.0.0.1:8787/tdata/Orders",
            &headers,
            r#"{"id":"order-local-no-header","Customer":"Lin"}"#,
        )
        .await
        .expect("local create should use typed tenant context");
    assert_eq!(status, StatusCode::CREATED.as_u16(), "{body}");

    let (status, body) = host
        .http_call(
            "GET",
            "http://127.0.0.1:8787/tdata/Orders('order-local-no-header')",
            &headers,
            "",
        )
        .await
        .expect("local read should use typed tenant context");
    assert_eq!(status, StatusCode::OK.as_u16(), "{body}");
    let fetched: serde_json::Value = serde_json::from_str(&body).expect("fetched JSON");
    assert_eq!(fetched["fields"]["Customer"], "Lin");
}

#[tokio::test]
async fn allowlisted_public_tdata_calls_use_odata_handlers() {
    let mut state = test_state();
    state.local_tdata_hosts = Arc::new(BTreeSet::from(["temper.example".to_string()]));
    let host = LocalTDataWasmHost::new(
        state,
        temper_runtime::tenant::TenantId::default(),
        Some(&SecurityContext::system()),
        Arc::new(FailingHost),
    );
    let headers = vec![
        ("x-tenant-id".to_string(), "default".to_string()),
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), "application/json".to_string()),
    ];

    let (status, body) = host
        .http_call(
            "POST",
            "https://temper.example/tdata/Orders",
            &headers,
            r#"{"id":"order-public-local-1","Customer":"Grace"}"#,
        )
        .await
        .expect("allowlisted public host should dispatch locally");
    assert_eq!(status, StatusCode::CREATED.as_u16());
    let created: serde_json::Value = serde_json::from_str(&body).expect("created JSON");
    assert_eq!(created["entity_id"], "order-public-local-1");

    let (status, body) = host
        .http_call(
            "GET",
            "https://temper.example/tdata/Orders('order-public-local-1')",
            &headers,
            "",
        )
        .await
        .expect("allowlisted public host read should dispatch locally");
    assert_eq!(status, StatusCode::OK.as_u16());
    let fetched: serde_json::Value = serde_json::from_str(&body).expect("fetched JSON");
    assert_eq!(fetched["fields"]["Customer"], "Grace");
}

#[path = "local_tdata_host_test/delegation_tests.rs"]
mod delegation_tests;
