//! Real WASM engine → native strict actor callback proof; no external providers.
use serde_json::json;
use temper_runtime::{ActorSystem, tenant::TenantId};
use temper_server::{
    ServerState, build_router,
    registry::{EntityVerificationResult, SpecRegistry, VerificationStatus},
};
use temper_spec::csdl::parse_csdl;

const SPEC: &str = r#"
[automaton]
name="Job"
states=["Idle","Pending","Done","Failed"]
initial="Idle"
strict_action_params=true
[[state]]
name="revision"
type="counter"
initial="1"
[[action]]
name="Run"
from=["Idle"]
to="Pending"
params=[]
effect="trigger local_job"
[[action]]
name="Complete"
from=["Pending"]
to="Done"
params=["observed","expected_revision"]
[[action.constraints]]
kind="param_equals_field"
param="expected_revision"
field="revision"
[[action]]
name="Fail"
from=["Pending"]
to="Failed"
params=["error"]
[[integration]]
name="local_job"
trigger="local_job"
type="wasm"
module="local_job"
on_success="Complete"
on_failure="Fail"
"#;
const CSDL: &str = r#"<?xml version="1.0"?><edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx"><edmx:DataServices><Schema Namespace="Test" xmlns="http://docs.oasis-open.org/odata/ns/edm"><EntityType Name="Job"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType><EntityContainer Name="Container"><EntitySet Name="Jobs" EntityType="Test.Job"/></EntityContainer></Schema></edmx:DataServices></edmx:Edmx>"#;

#[tokio::test]
async fn strict_native_callbacks_run_through_the_actual_wasm_engine() {
    for (success, expected_revision, status) in [
        (true, 1, "Done"),
        (true, 0, "Pending"),
        (false, 1, "Failed"),
    ] {
        let mut registry = SpecRegistry::new();
        registry.register_tenant(
            "default",
            parse_csdl(CSDL).unwrap(),
            CSDL.into(),
            &[("Job", SPEC)],
        );
        registry.set_verification_status(
            &TenantId::default(),
            "Job",
            VerificationStatus::Completed(EntityVerificationResult {
                all_passed: true,
                levels: vec![],
                verified_at: "2026-09-06T00:00:00Z".into(),
            }),
        );
        let state = ServerState::from_registry(ActorSystem::new("strict-wasm-engine"), registry);
        state
            .authz
            .reload_tenant_policies("default", "permit(principal, action, resource);")
            .unwrap();
        let payload = json!({"action":"Complete","params":{"observed":"real engine","expected_revision":expected_revision,"unrelated":"generated"},"success":success,"error":if success {""} else {"local execution failed"}}).to_string();
        let data = payload
            .bytes()
            .map(|byte| format!("\\{byte:02x}"))
            .collect::<String>();
        let wat = format!(
            r#"(module
            (import "env" "host_set_result" (func $result (param i32 i32)))
            (memory (export "memory") 1)
            (data (i32.const 0) "{data}")
            (func (export "run") (param i32 i32) (result i32)
                i32.const 0 i32.const {} call $result i32.const 0))"#,
            payload.len()
        );
        let hash = state.wasm_engine.compile_and_cache(wat.as_bytes()).unwrap();
        let tenant = TenantId::default();
        state
            .wasm_module_registry
            .write()
            .unwrap()
            .register(&tenant, "local_job", &hash);
        // Authentication is a test fixture; HTTP, policy, WASM and native actors are real.
        let app = build_router(state.clone()).layer(axum::middleware::from_fn(local_identity));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::new();
        let created = client
            .post(format!("{base}/tdata/Jobs"))
            .json(&json!({"Id":"job"}))
            .send()
            .await
            .unwrap();
        assert_eq!(created.status(), reqwest::StatusCode::CREATED);
        let response = client
            .post(format!(
                "{base}/tdata/Jobs('job')/Test.Run?await_integration=true"
            ))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if expected_revision == 0 {
                reqwest::StatusCode::CONFLICT
            } else {
                reqwest::StatusCode::OK
            }
        );
        let observed: serde_json::Value = client
            .get(format!("{base}/tdata/Jobs('job')"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(observed["status"], status);
        assert!(observed["fields"].get("unrelated").is_none());
        assert!(observed["fields"].get("duration_ms").is_none());
        if status == "Done" {
            assert_eq!(observed["fields"]["observed"], "real engine");
        }
        if status == "Failed" {
            assert!(
                observed["fields"]["error"]
                    .as_str()
                    .unwrap()
                    .contains("local execution failed")
            );
        }
        server.abort();
        if status == "Pending" {
            assert!(
                state
                    .entity_observe_log
                    .lock()
                    .unwrap()
                    .values()
                    .flatten()
                    .any(|event| event.event_name == "integration_callback_rejected")
            );
        }
    }
}

async fn local_identity(
    mut request: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            temper_authz::SecurityContext::from_resolved_identity(
                "local-strict-test",
                "test-agent",
                None,
            ),
        ));
    next.run(request).await
}
