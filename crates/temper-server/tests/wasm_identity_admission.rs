#![cfg(feature = "observe")]
//! Capability probe: compiled WASM, production host chain, OData and Cedar.
use serde_json::json;
use temper_authz::SecurityContext;
use temper_runtime::{ActorSystem, tenant::TenantId};
use temper_server::{
    ServerState, StorageStack,
    registry::{EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus},
    request_context::AgentContext,
    state::DispatchExtOptions,
};

const CHILD: &str = r#"
[automaton]
name = "Child"
states = ["Requested", "Active", "Removed"]
initial = "Requested"
strict_action_params = true
allow_indefinite_states = ["Requested", "Active", "Removed"]
[[state]]
name = "owner_id"
type = "string"
initial = ""
[[state]]
name = "instance_id"
type = "string"
initial = ""
[[action]]
name = "Create"
from = ["Requested"]
to = "Active"
params = [{name="owner_id", type="string", source="authenticated_subject"}, "instance_id"]
[[action]]
name = "Remove"
from = ["Active"]
to = "Removed"
[[action]]
name = "ModuleOnly"
from = ["Removed"]
"#;
fn worker() -> String {
    let fields = [
        "query_status",
        "query_body",
        "denied_read_status",
        "remove_status",
        "module_status",
        "module_body",
        "shared_secret_ok",
        "unrelated_secret_denied",
    ];
    let mut s = String::from(
        r#"
[automaton]
name = "Worker"
states = ["Idle", "Working", "Done", "Failed"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Working", "Done", "Failed"]
"#,
    );
    for f in fields {
        s.push_str(&format!(
            "\n[[state]]\nname = \"{f}\"\ntype = \"string\"\ninitial = \"\"\n"
        ));
    }
    s.push_str(r#"
[[action]]
name = "Start"
from = ["Idle"]
to = "Working"
[[action.triggers]]
name = "probe"
kind = "wasm"
module = "capability-probe"
on_success = "Done"
on_failure = "Failed"
[[action]]
name = "Done"
from = ["Working"]
to = "Done"
params = ["query_status","query_body","denied_read_status","remove_status","module_status","module_body","shared_secret_ok","unrelated_secret_denied"]
[[action]]
name = "Failed"
from = ["Working"]
to = "Failed"
params = ["error", "error_message", "integration"]
"#);
    s
}
const CSDL: &str = r#"<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx"><edmx:DataServices><Schema Namespace="Probe" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Child"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/><Property Name="owner_id" Type="Edm.String"/><Property Name="instance_id" Type="Edm.String"/></EntityType>
<EntityType Name="Worker"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType>
<Action Name="Create" IsBound="true"><Parameter Name="bindingParameter" Type="Probe.Child"/><Parameter Name="owner_id" Type="Edm.String"/><Parameter Name="instance_id" Type="Edm.String"/></Action>
<Action Name="Remove" IsBound="true"><Parameter Name="bindingParameter" Type="Probe.Child"/></Action>
<Action Name="ModuleOnly" IsBound="true"><Parameter Name="bindingParameter" Type="Probe.Child"/></Action>
<EntityContainer Name="Container"><EntitySet Name="Children" EntityType="Probe.Child"/><EntitySet Name="Workers" EntityType="Probe.Worker"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;
const POLICIES: &str = r#"
permit(principal is Agent, action, resource is Worker);
permit(principal is Agent, action in [Action::"create", Action::"Create", Action::"list"], resource is Child);
permit(principal is Agent, action in [Action::"read", Action::"Remove"], resource is Child)
when { resource has owner_id && resource.owner_id == principal.id };
permit(principal is Agent, action == Action::"ModuleOnly", resource is Child)
when { context has module && context.module == "capability-probe" };
permit(principal == Agent::"capability-probe", action == Action::"http_call", resource == HttpEndpoint::"127.0.0.1");
permit(principal == Agent::"capability-probe", action == Action::"access_secret", resource == Secret::"webhook_token");
"#;
fn caller(name: &str) -> AgentContext {
    AgentContext {
        security_ctx: Some(SecurityContext::from_resolved_identity(
            name,
            "operator",
            Some(name),
        )),
        agent_id: Some(name.into()),
        ..Default::default()
    }
}
async fn fixture() -> (ServerState, tempfile::TempDir) {
    let tenant = TenantId::new("probe");
    let worker = worker();
    let mut registry = SpecRegistry::new();
    registry
        .try_register_tenant(
            "probe",
            temper_spec::csdl::parse_csdl(CSDL).unwrap(),
            CSDL.into(),
            &[("Child", CHILD), ("Worker", &worker)],
        )
        .unwrap();
    for (kind, ioa) in [("Child", CHILD), ("Worker", worker.as_str())] {
        let verification = temper_verify::cascade::VerificationCascade::from_ioa(ioa)
            .with_sim_seeds(2)
            .with_prop_test_cases(20)
            .run();
        assert!(verification.all_passed, "{kind} verification failed");
        registry.set_verification_status(
            &tenant,
            kind,
            VerificationStatus::Completed(EntityVerificationResult {
                all_passed: verification.all_passed,
                levels: verification
                    .levels
                    .into_iter()
                    .map(|l| EntityLevelSummary {
                        level: l.level.to_string(),
                        passed: l.passed,
                        summary: l.summary,
                        details: None,
                    })
                    .collect(),
                verified_at: "2026-10-01T00:00:00Z".into(),
            }),
        );
    }
    let dir = tempfile::tempdir().unwrap();
    let store = temper_store_turso::TursoEventStore::new(
        &format!("file:{}", dir.path().join("probe.db").display()),
        None,
    )
    .await
    .unwrap();
    let mut state = ServerState::from_registry(ActorSystem::new("wasm-capability-probe"), registry);
    state.data_dir = dir.path().into();
    state.set_storage_stack(StorageStack::from_turso(store));
    state
        .authz
        .reload_tenant_policies("probe", POLICIES)
        .unwrap();
    let vault = temper_server::secrets::vault::SecretsVault::new(&[42; 32]);
    vault.cache_secret("probe", "webhook_token", "fixture-token".into());
    vault.cache_secret("probe", "unrelated_token", "must-not-read".into());
    state.secrets_vault = Some(std::sync::Arc::new(vault));
    let wasm = include_bytes!("fixtures/wasm-identity/identity.wasm");
    let hash = state.wasm_engine.compile_and_cache(wasm).unwrap();
    state
        .wasm_module_registry
        .write()
        .unwrap()
        .register(&tenant, "capability-probe", &hash);
    for (id, owner) in [("mine", "alice"), ("other", "bob")] {
        let r = state
            .dispatch_tenant_action(
                &tenant,
                "Child",
                id,
                "Create",
                json!({"instance_id":"i"}),
                &caller(owner),
            )
            .await
            .unwrap();
        assert!(r.success, "{:?}", r.error);
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    (state, dir)
}
#[tokio::test(flavor = "multi_thread")]
async fn compiled_wasm_queries_and_calls_actions_with_authenticated_authority() {
    let (state, _dir) = fixture().await;
    let tenant = TenantId::new("probe");
    let mut requester = caller("alice");
    requester
        .security_ctx
        .as_mut()
        .unwrap()
        .context_attrs
        .insert("module".into(), json!("previous-module"));
    let r = state
        .dispatch_tenant_action_ext(
            &tenant,
            "Worker",
            "work",
            "Start",
            json!({}),
            DispatchExtOptions {
                agent_ctx: &requester,
                await_integration: true,
                await_reactions: true,
            },
        )
        .await
        .unwrap();
    assert!(r.success, "{:?}", r.error);
    println!(
        "WASM probe status={} fields={}",
        r.state.status,
        json!(r.state.fields)
    );
    assert_eq!(r.state.status, "Done");
    assert_eq!(r.state.fields["shared_secret_ok"], "true");
    assert_eq!(r.state.fields["unrelated_secret_denied"], "true");
    assert_eq!(r.state.fields["query_status"], "200");
    assert_eq!(r.state.fields["denied_read_status"], "403");
    assert_eq!(r.state.fields["remove_status"], "200");
    assert_eq!(
        state
            .get_tenant_entity_state(&tenant, "Child", "mine")
            .await
            .unwrap()
            .state
            .status,
        "Removed"
    );
    assert_eq!(
        state
            .get_tenant_entity_state(&tenant, "Child", "other")
            .await
            .unwrap()
            .state
            .status,
        "Active"
    );
    let query: serde_json::Value =
        serde_json::from_str(r.state.fields["query_body"].as_str().unwrap()).unwrap();
    assert_eq!(query["value"].as_array().unwrap().len(), 1);
    assert_eq!(query["value"][0]["entity_id"], "mine");
    // Probe separately from ordinary caller authority: integration-only calls
    // need a kernel-authenticated module identity that a direct caller cannot forge.
    assert_eq!(
        r.state.fields["module_status"], "200",
        "Cedar must receive the authenticated module identity on local OData calls"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn wasm_http_endpoint_can_admit_an_action_as_the_authenticated_caller() {
    use axum::{
        Extension,
        body::{Body, to_bytes},
        extract::State,
        http::{HeaderMap, Method},
    };
    use temper_authz::AuthenticatedRequestContext;
    use temper_server::http_endpoint::route_from_entity_fields;
    let (state, _dir) = fixture().await;
    let tenant = TenantId::new("probe");
    state
        .http_endpoint_tables
        .table_for(&tenant)
        .await
        .replace(vec![route_from_entity_fields("probe-http", &json!({
            "PathPrefix":"/probe", "Methods":"GET", "IntegrationModule":"capability-probe", "RequiresAuth":true,
            "AdmissionActions":json!([{"name":"child", "entity_set":"Children", "entity_id":"mine", "action":"Probe.Remove"}]).to_string()
        })).unwrap()])
        .await;
    let response = temper_server::http_endpoint_fallback(
        State(state.clone()),
        Some(Extension(AuthenticatedRequestContext::new(
            tenant,
            caller("alice").security_ctx.unwrap(),
        ))),
        None,
        Method::GET,
        "/probe".parse().unwrap(),
        HeaderMap::new(),
        Body::empty(),
    )
    .await;
    let status = response.status();
    let body = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    println!(
        "HTTP WASM probe status={status} body={}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(status.as_u16(), 200);
    let report: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(report["shared_secret_ok"], "true");
    assert_eq!(report["unrelated_secret_denied"], "true");
    assert_eq!(
        report["remove_status"], "403",
        "endpoint guest retains module authority; caller authority is used only for declared admission"
    );
    assert_eq!(
        report["admitted"]["child"]["status"], "Removed",
        "Kernel must run the declared action as Alice before starting WASM"
    );
}

/// The router resolves its module again right before invoking. An install can
/// replace and drop the compiled version while admission runs, and a module
/// restored at boot is only compiled on first use.
#[tokio::test(flavor = "multi_thread")]
async fn endpoint_runs_a_registered_module_that_is_not_compiled() {
    let (state, _dir) = fixture().await;
    let tenant = TenantId::new("probe");
    let wasm = include_bytes!("fixtures/wasm-identity/identity.wasm");
    let hash = temper_wasm::WasmEngine::hash_module(wasm);
    state
        .upsert_wasm_module("probe", "capability-probe", wasm, &hash, "bundled")
        .await
        .unwrap();
    assert!(state.wasm_engine.evict(&hash));
    state
        .http_endpoint_tables
        .table_for(&tenant)
        .await
        .replace(vec![temper_server::http_endpoint::route_from_entity_fields("probe-http", &json!({
            "PathPrefix":"/probe", "Methods":"GET", "IntegrationModule":"capability-probe", "RequiresAuth":true,
            "AdmissionActions":json!([{"name":"child", "entity_set":"Children", "entity_id":"mine", "action":"Probe.Remove"}]).to_string()
        })).unwrap()])
        .await;
    let response = temper_server::http_endpoint_fallback(
        axum::extract::State(state.clone()),
        Some(axum::Extension(
            temper_authz::AuthenticatedRequestContext::new(
                tenant,
                caller("alice").security_ctx.unwrap(),
            ),
        )),
        None,
        axum::http::Method::GET,
        "/probe".parse().unwrap(),
        axum::http::HeaderMap::new(),
        axum::body::Body::empty(),
    )
    .await;
    assert_eq!(response.status().as_u16(), 200);
    assert!(state.wasm_engine.is_cached(&hash));
}

#[tokio::test(flavor = "multi_thread")]
async fn module_secret_is_independent_of_requesting_user() {
    let (state, _dir) = fixture().await;
    for user in ["alice", "bob"] {
        let r = state
            .dispatch_tenant_action_ext(
                &TenantId::new("probe"),
                "Worker",
                user,
                "Start",
                json!({}),
                DispatchExtOptions {
                    agent_ctx: &caller(user),
                    await_integration: true,
                    await_reactions: true,
                },
            )
            .await
            .unwrap();
        assert!(r.success);
        assert_eq!(r.state.status, "Done");
        assert_eq!(r.state.fields["shared_secret_ok"], "true");
        assert_eq!(r.state.fields["unrelated_secret_denied"], "true");
    }
}

async fn endpoint_call(state: &ServerState, user: &str) -> axum::response::Response {
    temper_server::http_endpoint_fallback(
        axum::extract::State(state.clone()),
        Some(axum::Extension(
            temper_authz::AuthenticatedRequestContext::new(
                TenantId::new("probe"),
                caller(user).security_ctx.unwrap(),
            ),
        )),
        None,
        axum::http::Method::GET,
        "/probe/mine".parse().unwrap(),
        axum::http::HeaderMap::from_iter([
            (
                axum::http::HeaderName::from_static("x-agent-id"),
                "alice".parse().unwrap(),
            ),
            (
                axum::http::HeaderName::from_static("idempotency-key"),
                "same-request".parse().unwrap(),
            ),
        ]),
        axum::body::Body::empty(),
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn endpoint_rejects_wrong_owner_and_rechecks_state_before_wasm() {
    let (state, _dir) = fixture().await;
    let route = temper_server::http_endpoint::route_from_entity_fields("probe", &json!({
        "PathPrefix":"/probe/{id}", "Methods":"GET", "IntegrationModule":"capability-probe", "RequiresAuth":true,
        "AdmissionActions":json!([{"name":"child", "entity_set":"Children", "entity_id":"{id}", "action":"Probe.Remove"}]).to_string()
    })).unwrap();
    state
        .http_endpoint_tables
        .table_for(&TenantId::new("probe"))
        .await
        .replace(vec![route])
        .await;
    // A forged Alice header cannot override Bob's authenticated context.
    assert_eq!(endpoint_call(&state, "bob").await.status(), 403);
    assert_eq!(
        state
            .get_tenant_entity_state(&TenantId::new("probe"), "Child", "mine")
            .await
            .unwrap()
            .state
            .status,
        "Active"
    );
    assert_eq!(endpoint_call(&state, "alice").await.status(), 200);
    // The same idempotency key must not bypass current IOA state checks.
    let response = endpoint_call(&state, "alice").await;
    assert!(response.status().is_client_error());
}

#[test]
fn malformed_admission_configuration_is_not_silently_ignored() {
    for invalid in [
        json!(true),
        json!("{}"),
        json!("[{\"unknown\":1}]"),
        json!("not json"),
    ] {
        let route = temper_server::http_endpoint::route_from_entity_fields(
            "probe",
            &json!({
                "PathPrefix":"/probe", "Methods":"GET", "IntegrationModule":"capability-probe", "AdmissionActions":invalid
            }),
        );
        assert!(route.is_none());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_module_does_not_execute_admission() {
    let (state, _dir) = fixture().await;
    let route = temper_server::http_endpoint::route_from_entity_fields("probe", &json!({
        "PathPrefix":"/probe/{id}", "Methods":"GET", "IntegrationModule":"not-installed", "RequiresAuth":true,
        "AdmissionActions":json!([{"name":"child", "entity_set":"Children", "entity_id":"{id}", "action":"Probe.Remove"}]).to_string()
    })).unwrap();
    state
        .http_endpoint_tables
        .table_for(&TenantId::new("probe"))
        .await
        .replace(vec![route])
        .await;
    assert_eq!(endpoint_call(&state, "alice").await.status(), 503);
    assert_eq!(
        state
            .get_tenant_entity_state(&TenantId::new("probe"), "Child", "mine")
            .await
            .unwrap()
            .state
            .status,
        "Active"
    );
}

async fn route_call(state: &ServerState, path: &str) -> (u16, String, std::time::Duration) {
    let started = std::time::Instant::now();
    let response = temper_server::http_endpoint_fallback(
        axum::extract::State(state.clone()),
        Some(axum::Extension(
            temper_authz::AuthenticatedRequestContext::new(
                TenantId::new("probe"),
                caller("alice").security_ctx.unwrap(),
            ),
        )),
        None,
        axum::http::Method::GET,
        path.parse().unwrap(),
        axum::http::HeaderMap::new(),
        axum::body::Body::empty(),
    )
    .await;
    let status = response.status().as_u16();
    let body = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    (
        status,
        String::from_utf8_lossy(&body).into_owned(),
        started.elapsed(),
    )
}

async fn install_route(state: &ServerState, path: &str, module: &str, timeout_secs: u32) {
    let route = temper_server::http_endpoint::route_from_entity_fields(
        module,
        &json!({
            "PathPrefix": path, "Methods": "GET", "IntegrationModule": module, "RequiresAuth": true,
            "TimeoutSecs": timeout_secs,
        }),
    )
    .unwrap();
    state
        .http_endpoint_tables
        .table_for(&TenantId::new("probe"))
        .await
        .replace(vec![route])
        .await;
}

/// After a restart the module registry is restored from the store and nothing is compiled yet: a
/// route compiles its module on first use, as an entity integration does.
#[tokio::test(flavor = "multi_thread")]
async fn a_route_compiles_its_module_on_first_use_after_a_restart() {
    let (state, _dir) = fixture().await;
    let wasm = include_bytes!("fixtures/wasm-identity/identity.wasm");
    let hash = temper_wasm::WasmEngine::hash_module(wasm);
    state
        .upsert_wasm_module("probe", "capability-probe", wasm, &hash, "upload")
        .await
        .unwrap();
    assert!(
        state.wasm_engine.evict(&hash),
        "the fixture had compiled it"
    );
    install_route(&state, "/probe", "capability-probe", 5).await;
    let (status, body, _) = route_call(&state, "/probe").await;
    assert_eq!(
        status, 200,
        "the route answers after compiling its module: {body}"
    );
    assert!(state.wasm_engine.is_cached(&hash));
}

/// A module the store does not hold is refused at once, not at the route's timeout.
#[tokio::test(flavor = "multi_thread")]
async fn a_route_whose_module_cannot_be_loaded_answers_at_once() {
    let (state, _dir) = fixture().await;
    state.wasm_module_registry.write().unwrap().register(
        &TenantId::new("probe"),
        "ghost",
        &"0".repeat(64),
    );
    install_route(&state, "/ghost", "ghost", 20).await;
    let (status, body, took) = route_call(&state, "/ghost").await;
    assert_eq!(status, 503, "{body}");
    assert!(
        took < std::time::Duration::from_secs(5),
        "answered in {took:?}"
    );
}

/// A module that ends without submitting a response head is answered at once, not at the route's timeout.
#[tokio::test(flavor = "multi_thread")]
async fn a_module_that_ends_without_answering_is_answered_at_once() {
    let (state, _dir) = fixture().await;
    let silent = state
        .wasm_engine
        .compile_and_cache(
            br#"(module (memory (export "memory") 1) (func (export "run") (param i32 i32) (result i32) unreachable))"#,
        )
        .unwrap();
    state.wasm_module_registry.write().unwrap().register(
        &TenantId::new("probe"),
        "silent",
        &silent,
    );
    install_route(&state, "/silent", "silent", 20).await;
    let (status, body, took) = route_call(&state, "/silent").await;
    assert_eq!(status, 502, "{body}");
    assert!(
        took < std::time::Duration::from_secs(5),
        "answered in {took:?}"
    );
}

/// A span's name, and its parent's (None for a root).
type NamedSpan = (String, Option<String>);

/// Each span made, in order.
#[derive(Clone, Default)]
struct SpanParents(std::sync::Arc<std::sync::Mutex<Vec<NamedSpan>>>);

impl<S> tracing_subscriber::Layer<S> for SpanParents
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        _attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let span = ctx.span(id).expect("a new span is registered");
        let parent = span.parent().map(|parent| parent.name().to_string());
        self.0
            .lock()
            .unwrap()
            .push((span.name().to_string(), parent));
    }
}

/// A route's module runs inside the request's span: the request is one trace,
/// never a module call that starts a trace of its own.
#[tokio::test]
async fn a_routes_module_runs_inside_the_request_span() {
    use tracing::Instrument as _;
    use tracing_subscriber::layer::SubscriberExt as _;
    let (state, _dir) = fixture().await;
    install_route(&state, "/probe", "capability-probe", 5).await;
    let spans = SpanParents::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(spans.clone()));
    let (status, body, _) = route_call(&state, "/probe")
        .instrument(tracing::info_span!("http.request"))
        .await;
    assert_eq!(status, 200, "{body}");
    let spans = spans.0.lock().unwrap().clone();
    assert!(
        spans.iter().any(|(name, _)| name == "wasm.invoke"),
        "the module ran: {spans:?}"
    );
    // The outbox's persist is a tracing root that continues its request's
    // trace through OpenTelemetry, so only the module's own spans are checked.
    let module_roots: Vec<&str> = spans
        .iter()
        .filter(|(name, parent)| parent.is_none() && name.starts_with("wasm."))
        .map(|(name, _)| name.as_str())
        .collect();
    assert!(module_roots.is_empty(), "{spans:?}");
}
