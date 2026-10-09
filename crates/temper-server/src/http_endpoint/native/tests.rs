use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use temper_runtime::{ActorSystem, tenant::TenantId};
use tower::ServiceExt;

struct Echo(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl HttpTransport for Echo {
    async fn send(&self, req: TransportRequest) -> Result<Response, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        assert!(req.headers.get("authorization").is_none());
        assert!(req.headers.get("x-tenant-id").is_none());
        assert_eq!(req.headers["if-match"], "version-7");
        assert_eq!(req.uri.query(), Some("$filter=Value%20gt%205"));
        assert_eq!(req.admitted["target"]["status"], "Ready");
        Ok(Response::builder()
            .status(201)
            .header("etag", "version-8")
            .header("odata-version", "4.0")
            .body(Body::from(req.body))
            .unwrap())
    }
}
fn fixture() -> (ServerState, Arc<AtomicUsize>) {
    let xml = r#"<edmx:Edmx xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx" Version="4.0"><edmx:DataServices><Schema Namespace="Example" xmlns="http://docs.oasis-open.org/odata/ns/edm"><EntityType Name="Target"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String"/><Property Name="Status" Type="Edm.String"/></EntityType><EntityContainer Name="Service"><EntitySet Name="Targets" EntityType="Example.Target"/></EntityContainer><Action Name="Serve" IsBound="true"><Parameter Name="bindingParameter" Type="Example.Target"/><ReturnType Type="Example.Target"/></Action></Schema></edmx:DataServices></edmx:Edmx>"#;
    let spec = r#"
[automaton]
name = "Target"
states = ["Ready","Stopped"]
initial = "Ready"
[[action]]
name = "Serve"
kind = "input"
from = ["Ready"]
[[action]]
name = "Stop"
kind = "input"
from = ["Ready"]
to = "Stopped"
"#;
    let mut state = ServerState::with_specs(
        ActorSystem::new("native-http"),
        temper_spec::csdl::parse_csdl(xml).unwrap(),
        xml.into(),
        BTreeMap::from([("Target".into(), spec.into())]),
    )
    .unwrap();
    state.authz.reload_tenant_policies("default",r#"permit(principal == Agent::"alice", action == Action::"Serve", resource is Target);"#).unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let mut registry = TransportRegistry::default();
    registry.register("echo", Arc::new(Echo(count.clone())));
    state.http_transports = Arc::new(registry);
    (state, count)
}
// Route admission is an ordinary bound action, not an entity initializer.
async fn create_target(state: &ServerState, id: &str) {
    let created = state
        .get_or_create_tenant_entity(&TenantId::default(), "Target", id, json!({}))
        .await
        .unwrap();
    assert_eq!(created.state.status, "Ready");
}

async fn assert_serve_ran(state: &ServerState, id: &str) {
    let current = state
        .get_tenant_entity_state(&TenantId::default(), "Target", id)
        .await
        .unwrap();
    assert_eq!(current.state.events.back().unwrap().action, "Serve");
}

async fn install(state: &ServerState, config: Value) {
    let route = super::super::route_from_entity_fields(
        "endpoint",
        &json!({
            "PathPrefix":"/things/{id}/tdata","Methods":"GET,POST","IntegrationModule":"echo",
            "RequiresAuth":true,"NativeConfig":config.to_string()
        }),
    )
    .unwrap();
    state
        .http_endpoint_tables
        .table_for(&TenantId::default())
        .await
        .replace(vec![route])
        .await;
}
fn config() -> Value {
    json!({"transport":"echo","actions":[{"name":"target","entity_set":"Targets","entity_id":"{id}","action":"Example.Serve"}]})
}
fn request(user: &str) -> axum::http::Request<Body> {
    let identity = temper_authz::SecurityContext::from_verified_jwt(
        user,
        temper_authz::PrincipalKind::Agent,
        Some("test"),
        Some(user),
        None,
        None,
    );
    axum::http::Request::builder()
        .method("POST")
        .uri("/things/one/tdata/Items?$filter=Value%20gt%205")
        .header("authorization", "Bearer must-not-leak")
        .header("x-tenant-id", "forged")
        .header("if-match", "version-7")
        .header("idempotency-key", "repeat-this")
        .extension(AuthenticatedRequestContext::new(
            TenantId::default(),
            identity,
        ))
        .body(Body::from(vec![0, 1, 255, 13, 10]))
        .unwrap()
}
#[tokio::test]
async fn native_route_runs_ioa_and_preserves_http() {
    let (state, count) = fixture();
    create_target(&state, "one").await;
    install(&state, config()).await;
    let response = crate::build_router(state.clone())
        .oneshot(request("alice"))
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(status, 201, "{}", String::from_utf8_lossy(&body));
    assert_eq!(headers["etag"], "version-8");
    assert_eq!(headers["odata-version"], "4.0");
    assert_eq!(&body[..], &[0, 1, 255, 13, 10]);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    // The upstream 201 is preserved; admission itself only acts on the existing target.
    assert_serve_ran(&state, "one").await;
}
#[tokio::test]
async fn denied_caller_never_reaches_transport() {
    let (state, count) = fixture();
    install(&state, config()).await;
    let response = crate::build_router(state.clone())
        .oneshot(request("bob"))
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert!(!state.entity_exists(&TenantId::default(), "Target", "one"));
}
#[tokio::test]
async fn repeated_request_rechecks_current_ioa_state() {
    let (state, count) = fixture();
    create_target(&state, "one").await;
    install(&state, config()).await;
    assert_eq!(
        crate::build_router(state.clone())
            .oneshot(request("alice"))
            .await
            .unwrap()
            .status(),
        201
    );
    let response = state
        .dispatch_tenant_action(
            &TenantId::default(),
            "Target",
            "one",
            "Stop",
            json!({}),
            &crate::request_context::AgentContext::for_service("test"),
        )
        .await
        .unwrap();
    assert!(response.success);
    let response = crate::build_router(state.clone())
        .oneshot(request("alice"))
        .await
        .unwrap();
    assert!(!response.status().is_success());
    assert_eq!(count.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn empty_admission_cannot_bypass_specs() {
    let (state, count) = fixture();
    install(&state, json!({"transport":"echo","actions":[]})).await;
    let response = crate::build_router(state)
        .oneshot(request("alice"))
        .await
        .unwrap();
    assert_eq!(response.status(), 500);
    assert_eq!(count.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn all_gates_must_pass() {
    let (state, count) = fixture();
    let mut config = config();
    config["actions"].as_array_mut().unwrap().push(json!({"name":"second","entity_set":"Targets","entity_id":"two","action":"Example.MissingAction"}));
    create_target(&state, "one").await;
    create_target(&state, "two").await;
    install(&state, config).await;
    let response = crate::build_router(state.clone())
        .oneshot(request("alice"))
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    assert_serve_ran(&state, "one").await;
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn malformed_later_admission_cannot_create_earlier_entity() {
    for later in [
        json!({"name":"target","entity_set":"Targets","entity_id":"two","action":"Example.Serve"}),
        json!({"name":"second","entity_set":"Targets/invalid","entity_id":"two","action":"Example.Serve"}),
        json!({"name":"second","entity_set":"Targets","entity_id":"two","action":"Serve"}),
        json!({"name":"second","entity_set":"Targets","entity_id":"two","action":"Example..Serve"}),
        json!({"name":"second","entity_set":"Targets","entity_id":"{absent}","action":"Example.Serve"}),
        json!({"name":"second","entity_set":"Targets","entity_id":"two","action":"Example.Serve","params":{"value":"{absent}"}}),
    ] {
        let (state, count) = fixture();
        let mut c = config();
        c["actions"].as_array_mut().unwrap().push(later);
        install(&state, c).await;
        let response = crate::build_router(state.clone())
            .oneshot(request("alice"))
            .await
            .unwrap();
        assert_eq!(response.status(), 500);
        assert_eq!(count.load(Ordering::SeqCst), 0);
        assert!(
            !state.entity_exists(&TenantId::default(), "Target", "one"),
            "invalid declarations must be rejected before the first action"
        );
    }
}

#[tokio::test]
async fn oversized_request_cannot_run_admission() {
    let (state, count) = fixture();
    install(&state, config()).await;
    let mut req = request("alice");
    *req.body_mut() = Body::from(vec![0; 8 * 1024 * 1024 + 1]);
    let response = crate::build_router(state.clone())
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(response.status(), 413);
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert!(!state.entity_exists(&TenantId::default(), "Target", "one"));
}

#[tokio::test]
async fn quoted_parentheses_in_ids_are_data() {
    for (encoded, decoded) in [("a%29b", "a)b"), ("a%28b", "a(b")] {
        let (state, count) = fixture();
        create_target(&state, decoded).await;
        install(&state, config()).await;
        let mut req = request("alice");
        *req.uri_mut() = format!("/things/{encoded}/tdata/Items?$filter=Value%20gt%205")
            .parse()
            .unwrap();
        let response = crate::build_router(state.clone())
            .oneshot(req)
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        assert_eq!(status, 201, "{decoded}: {}", String::from_utf8_lossy(&body));
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(state.entity_exists(&TenantId::default(), "Target", decoded));
    }
}

#[tokio::test]
async fn pg_admission_is_rejected_before_enqueue() {
    let (mut state, count) = fixture();
    state.actor_backed_types.insert("Target".into());
    install(&state, config()).await;
    let response = crate::build_router(state.clone())
        .oneshot(request("alice"))
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert!(String::from_utf8_lossy(&body).contains("AdmissionRequiresCompletion"));
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert!(!state.entity_exists(&TenantId::default(), "Target", "one"));
}

struct Streaming;
#[async_trait::async_trait]
impl HttpTransport for Streaming {
    async fn send(&self, _req: TransportRequest) -> Result<Response, String> {
        let stream = async_stream::stream! {
            yield Ok::<_, std::io::Error>(Bytes::from_static(b"first"));
            tokio::time::sleep(Duration::from_secs(30)).await;
            yield Ok(Bytes::from_static(b"second"));
        };
        Ok(Response::new(Body::from_stream(stream)))
    }
}

#[tokio::test(start_paused = true)]
async fn response_stream_obeys_original_deadline() {
    let (mut state, _) = fixture();
    let mut registry = TransportRegistry::default();
    registry.register("echo", Arc::new(Streaming));
    state.http_transports = Arc::new(registry);
    create_target(&state, "one").await;
    install(&state, config()).await;
    let table = state
        .http_endpoint_tables
        .table_for(&TenantId::default())
        .await;
    let mut route = table
        .match_request("POST", "/things/one/tdata/Items")
        .await
        .unwrap()
        .route;
    route.timeout_secs = 1;
    table.replace(vec![route]).await;
    let response = crate::build_router(state)
        .oneshot(request("alice"))
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "admission must reach the transport");
    assert!(
        to_bytes(response.into_body(), 100).await.is_err(),
        "body must fail after the exchange deadline"
    );
}

#[tokio::test]
async fn response_stream_obeys_byte_budget() {
    let (state, _) = fixture();
    create_target(&state, "one").await;
    install(&state, config()).await;
    let table = state
        .http_endpoint_tables
        .table_for(&TenantId::default())
        .await;
    let mut route = table
        .match_request("POST", "/things/one/tdata/Items")
        .await
        .unwrap()
        .route;
    route.max_response_bytes = Some(3);
    table.replace(vec![route]).await;
    let response = crate::build_router(state)
        .oneshot(request("alice"))
        .await
        .unwrap();
    assert_eq!(response.status(), 201, "admission must reach the transport");
    assert!(
        to_bytes(response.into_body(), 100).await.is_err(),
        "five response bytes must exceed a three-byte budget"
    );
}

#[tokio::test]
async fn noncanonical_cedar_id_is_not_rewritten_to_bypass_authorization() {
    let (state, count) = fixture();
    install(&state, config()).await;
    let mut req = request("alice");
    *req.uri_mut() = "/things/a%27%29%2Fb/tdata/Items".parse().unwrap();
    let response = crate::build_router(state.clone())
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert!(String::from_utf8_lossy(&body).contains("invalid resource"));
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert!(!state.entity_exists(&TenantId::default(), "Target", "a')/b"));
}

#[path = "admission_existence_tests.rs"]
mod admission_existence;
