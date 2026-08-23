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

fn module_authz_context() -> temper_wasm::WasmAuthzContext {
    temper_wasm::WasmAuthzContext {
        tenant: TenantId::default().to_string(),
        module_name: "operate_arc_task_synthesis".to_string(),
        agent_id: Some("service:wasm-runtime".to_string()),
        session_id: None,
        entity_type: "ArcTaskSynthesis".to_string(),
        trigger_action: "RecordInitialActivated".to_string(),
    }
}

#[tokio::test]
async fn wasm_local_tdata_host_authorizes_as_module_across_reconstruction() {
    let state = test_state();
    state
        .authz
        .reload_tenant_policies(
            TenantId::default().as_str(),
            r#"permit(principal is Agent, action in [Action::"create", Action::"read"], resource is Order) when {
                principal has role &&
                principal.id == "operate_arc_task_synthesis" &&
                principal.role == "wasm_module"
            };"#,
        )
        .expect("module-only policy should parse");
    let wasm = module_authz_context();
    let host = LocalTDataWasmHost::new_for_wasm(
        state.clone(),
        TenantId::default(),
        &wasm,
        Arc::new(FailingHost),
    );
    let headers = vec![("content-type".to_string(), "application/json".to_string())];
    let (status, _) = host
        .http_call(
            "POST",
            "http://127.0.0.1:8787/tdata/Orders",
            &headers,
            r#"{"Id":"module-owned-order"}"#,
        )
        .await
        .expect("module-authorized create should stay in process");
    assert_eq!(status, StatusCode::CREATED.as_u16());

    // Reconstruct the host as recovery does and prove the typed module
    // authority, tenant, and durable entity remain stable.
    let recovered =
        LocalTDataWasmHost::new_for_wasm(state, TenantId::default(), &wasm, Arc::new(FailingHost));
    let (status, body) = recovered
        .http_call(
            "GET",
            "http://127.0.0.1:8787/tdata/Orders('module-owned-order')",
            &[],
            "",
        )
        .await
        .expect("recovered module-authorized read should stay in process");
    assert_eq!(status, StatusCode::OK.as_u16());
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["entity_id"],
        "module-owned-order"
    );
}

#[tokio::test]
async fn http_endpoint_host_does_not_delegate_local_tdata_to_the_ambient_caller() {
    let state = test_state();
    let module_http_permit = r#"
        permit(principal is Agent, action == Action::"http_call", resource is HttpEndpoint) when {
            principal has role &&
            principal.id == "operate_arc_task_synthesis" &&
            principal.role == "wasm_module"
        };
    "#;
    state
        .authz
        .reload_tenant_policies(
            TenantId::default().as_str(),
            &format!(
                r#"{module_http_permit}
                permit(principal is Agent, action == Action::"create", resource is Order) when {{
                    principal.id == "ambient-operator"
                }};"#
            ),
        )
        .expect("caller-only entity policy should parse");
    let invocation = temper_wasm::types::WasmInvocationContext {
        tenant: TenantId::default().to_string(),
        entity_type: "HttpEndpoint".to_string(),
        entity_id: "module-route".to_string(),
        trigger_action: "HandleHttp".to_string(),
        wasm_module: Some("operate_arc_task_synthesis".to_string()),
        trigger_params: serde_json::Value::Null,
        entity_state: serde_json::Value::Null,
        agent_id: Some("ambient-operator".to_string()),
        session_id: None,
        integration_config: BTreeMap::new(),
        trace_id: String::new(),
        workflow_root_entity_type: None,
        workflow_root_entity_id: None,
        workflow_run_id: None,
        http_request: None,
    };
    let build_host = || {
        super::super::authorized_http_endpoint_host(
            &state,
            &TenantId::default(),
            "operate_arc_task_synthesis",
            &invocation,
            state.http_stream_registry.clone(),
        )
        .expect("HTTP endpoint host should build")
    };
    let headers = vec![("content-type".to_string(), "application/json".to_string())];
    let (status, _) = build_host()
        .http_call(
            "POST",
            "http://127.0.0.1:8787/tdata/Orders",
            &headers,
            r#"{"Id":"caller-only-order"}"#,
        )
        .await
        .expect("local TData denial should return an HTTP response");
    assert_eq!(status, StatusCode::FORBIDDEN.as_u16());

    state
        .authz
        .reload_tenant_policies(
            TenantId::default().as_str(),
            &format!(
                r#"{module_http_permit}
                permit(principal is Agent, action == Action::"create", resource is Order) when {{
                    principal has role &&
                    principal.id == "operate_arc_task_synthesis" &&
                    principal.role == "wasm_module"
                }};"#
            ),
        )
        .expect("module-only entity policy should parse");
    let (status, _) = build_host()
        .http_call(
            "POST",
            "http://127.0.0.1:8787/tdata/Orders",
            &headers,
            r#"{"Id":"module-endpoint-order"}"#,
        )
        .await
        .expect("module-authorized local TData should return an HTTP response");
    assert_eq!(status, StatusCode::CREATED.as_u16());
}

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
    fn temper_data_request_budget(&self) -> usize {
        4096
    }

    fn temper_data_response_handle_budget(&self) -> usize {
        3
    }

    async fn temper_data_call(&self, request: &[u8]) -> Result<Vec<u8>, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(request.to_vec())
    }

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

#[tokio::test]
async fn local_tdata_wrapper_delegates_application_data_capability() {
    let calls = Arc::new(AtomicUsize::new(0));
    let delegate = Arc::new(CountingHost {
        calls: calls.clone(),
        stream_calls: Arc::new(AtomicUsize::new(0)),
    });
    let csdl = parse_csdl(ORDER_CSDL_XML).expect("valid CSDL");
    let state = ServerState::new(
        temper_runtime::ActorSystem::new("local-data-wrapper-test"),
        csdl,
        ORDER_CSDL_XML.into(),
    );
    let host = LocalTDataWasmHost::new(state, TenantId::new("alpha"), None, delegate);

    assert_eq!(host.temper_data_request_budget(), 4096);
    assert_eq!(host.temper_data_response_handle_budget(), 3);
    assert_eq!(host.temper_data_call(b"request").await.unwrap(), b"request");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

fn test_state() -> ServerState {
    let csdl = parse_csdl(ORDER_CSDL_XML).expect("test CSDL should parse");
    let system = ActorSystem::new("local-tdata-wasm-host-test");
    let mut specs = BTreeMap::new();
    specs.insert("Order".to_string(), ORDER_IOA.to_string());
    ServerState::with_specs(system, csdl, ORDER_CSDL_XML.to_string(), specs)
        .expect("test state should build")
}

fn permit_agents(state: &ServerState) {
    state
        .authz
        .reload_tenant_policies(
            TenantId::default().as_str(),
            "permit(principal is Agent, action, resource);",
        )
        .expect("agent-only policy should parse");
}

fn test_agent() -> SecurityContext {
    SecurityContext::from_resolved_identity("agent-1", "operator", None)
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
    let state = test_state();
    permit_agents(&state);
    let agent = test_agent();
    let host = LocalTDataWasmHost::new(
        state,
        temper_runtime::tenant::TenantId::default(),
        Some(&agent),
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
/// WITH the caller's typed authority. The delegate is `FailingHost`, so if the
/// helper regresses to no authority the `/tdata` call falls through to it and
/// the test fails — the silent-401 blob regression ARN-170 introduced.
#[tokio::test]
async fn direct_invocation_loopback_dispatches_in_process_with_caller_authority() {
    let state = test_state();
    state
        .authz
        .reload_tenant_policies(
            TenantId::default().as_str(),
            "permit(principal is Agent, action, resource);",
        )
        .expect("agent-only policy should parse");
    let caller = SecurityContext::from_resolved_identity("agent-1", "operator", None);
    let host = state.local_tdata_direct_host(&TenantId::default(), Arc::new(FailingHost), &caller);
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

/// A customer with no permit must not create via the blob_adapter loopback.
///
/// `test_state()` installs `system-platform:broad-permit`, so a System
/// loopback would return 201. The helper must carry the caller instead.
#[tokio::test]
async fn direct_invocation_loopback_does_not_run_as_system() {
    let state = test_state();
    let caller = customer_security_context("customer-1");
    let host = state.local_tdata_direct_host(&TenantId::default(), Arc::new(FailingHost), &caller);
    let headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), "application/json".to_string()),
    ];

    let (status, body) = host
        .http_call(
            "POST",
            "http://127.0.0.1:8787/tdata/Orders",
            &headers,
            r#"{"id":"system-elevated-order","Customer":"Eve"}"#,
        )
        .await
        .expect("loopback must stay in-process under the caller principal");
    assert_eq!(
        status,
        StatusCode::FORBIDDEN.as_u16(),
        "customer loopback must not inherit System, got {status}: {body}"
    );
    assert!(!state.entity_exists(&TenantId::default(), "Order", "system-elevated-order"));
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
    permit_agents(&state);
    let agent = test_agent();
    let host = LocalTDataWasmHost::new(
        state,
        temper_runtime::tenant::TenantId::default(),
        Some(&agent),
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
    permit_agents(&state);
    let agent = test_agent();
    let host = LocalTDataWasmHost::new(
        state,
        temper_runtime::tenant::TenantId::default(),
        Some(&agent),
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

/// ARN-243 / ADR-0166. The engine reads the tenant's content decision off the
/// host it is handed, and production hands it a three-layer stack:
/// `AuthorizedWasmHost(LocalTDataWasmHost(ProductionWasmHost))`. Only the
/// innermost host holds the flag, so every wrapper in between has to forward it.
/// A wrapper that does not silently disables the opt-in for every tenant —
/// fail-safe, but inert. Asserted on the real composition, because a test that
/// wraps `ProductionWasmHost` directly builds a stack production never uses and
/// passes while the real one drops the decision.
#[tokio::test]
async fn production_host_stack_forwards_the_llm_content_export_decision() {
    use temper_wasm::authorized_host::AuthorizedWasmHost;
    use temper_wasm::host_trait::ProductionWasmHost;

    for opted_in in [true, false] {
        let inner: Arc<dyn WasmHost> = Arc::new(
            ProductionWasmHost::new(std::collections::BTreeMap::new())
                .with_llm_content_export(opted_in),
        );
        let state = test_state();
        let agent = test_agent();
        let local_tdata: Arc<dyn WasmHost> = Arc::new(LocalTDataWasmHost::new(
            state,
            temper_runtime::tenant::TenantId::default(),
            Some(&agent),
            inner,
        ));
        assert_eq!(
            local_tdata.exports_llm_content(),
            opted_in,
            "LocalTDataWasmHost must forward the decision (opted_in={opted_in})"
        );

        let full_stack = AuthorizedWasmHost::new(
            local_tdata,
            test_state().wasm_authz_gate(),
            temper_wasm::WasmAuthzContext {
                tenant: TenantId::default().to_string(),
                module_name: "llm_caller".to_string(),
                agent_id: None,
                session_id: None,
                entity_type: "Order".to_string(),
                trigger_action: "SubmitOrder".to_string(),
            },
        );
        assert_eq!(
            full_stack.exports_llm_content(),
            opted_in,
            "the production host stack must carry the tenant's decision to the \
             engine (opted_in={opted_in})"
        );
    }
}
