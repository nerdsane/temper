//! Human setup exercised against the actual local Temper router and registry.

use super::*;
use crate::McpConfig;
use crate::client_requests::{ClientRequester, PendingClientRequests};
use std::os::unix::fs::PermissionsExt;
use temper_platform::{
    PlatformState, bootstrap_operator_credential, bootstrap_operator_credential_specs,
};
use temper_server::StorageStack;
use temper_store_turso::TursoEventStore;

#[tokio::test]
async fn human_setup_provisions_distinct_identity_on_real_temper() {
    let directory =
        std::env::temp_dir().join(format!("temper-setup-live-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let db = format!("file:{}", directory.join("events.db").display());
    let mut platform = PlatformState::new(None);
    platform.server.set_storage_stack(StorageStack::from_turso(
        TursoEventStore::new(&db, None).await.unwrap(),
    ));
    bootstrap_operator_credential_specs(&platform, "default")
        .await
        .unwrap();
    bootstrap_operator_credential(&platform, "fixture-operator-key", "default")
        .await
        .unwrap();
    let router = temper_platform::router::build_platform_router(platform);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let mut ctx = RuntimeContext::from_config(&McpConfig {
        temper_url: Some(format!("http://127.0.0.1:{port}")),
        temper_port: None,
        api_key: Some("fixture-operator-key".into()),
        agent_id: None,
        agent_type: None,
        session_id: None,
    })
    .unwrap();
    ctx.approver_key = None;
    ctx.agent_id = Some("operator".into());
    ctx.init_trajectory();
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let pending = PendingClientRequests::default();
    ctx.requester = Some(ClientRequester::new(tx, pending.clone()));
    let path = directory.join("requester.json");
    ctx.identity_file = Some(path.clone());
    ctx.client_supports_elicitation = true;
    ctx.elicit_approvals_enabled = true;
    let answer = async {
        let prompt = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(prompt["method"], "elicitation/create");
        assert!(!path.exists());
        pending.resolve(json!({"id":prompt["id"],"result":{
            "action":"accept", "content":{"setup":"configure_agent_identity"}
        }}));
    };
    let call = json!({"jsonrpc":"2.0","id":42,"method":"tools/call",
        "params":{"name":"setup_connection"}});
    let (result, ()) = tokio::join!(crate::protocol::dispatch_json_value(&mut ctx, call), answer);
    let envelope = result.unwrap();
    assert_eq!(envelope["id"], 42);
    assert_eq!(envelope["result"]["isError"], false, "{envelope}");
    let result: Value =
        serde_json::from_str(envelope["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(result["status"], "configured");
    assert_eq!(ctx.agent_type.as_deref(), Some(REQUESTER_TYPE));
    let audit = serde_json::to_value(ctx.trajectory.as_ref().unwrap().snapshot()).unwrap();
    assert!(audit.to_string().contains(ctx.agent_id.as_ref().unwrap()));
    assert!(!audit.to_string().contains("\"agent_id\":\"operator\""));
    assert_ne!(ctx.api_key, ctx.approver_key);
    let saved = load_identity(&path, &ctx.base_url, &ctx.identity_tenant).unwrap();
    assert_eq!(ctx.api_key.as_deref(), Some(saved.token.as_str()));
    let first_key = ctx.api_key.clone();
    let answer_again = async {
        let prompt = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        pending.resolve(json!({"id":prompt["id"],"result":{
            "action":"accept", "content":{"setup":"configure_agent_identity"}
        }}));
    };
    let (repeated, ()) = tokio::join!(setup_at(&mut ctx, &path), answer_again);
    assert!(
        repeated.is_ok(),
        "repeated setup must reuse identity: {repeated:?}"
    );
    assert_eq!(ctx.api_key, first_key);
    // The new requester must not acquire the operator's administration power.
    let response = ctx
        .http
        .get(format!("{}/api/tenants/default/policies", ctx.base_url))
        .bearer_auth(saved.token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
    prove_governance_flow(&mut ctx, &mut rx, &pending).await;
    server.abort();
    let _ = std::fs::remove_dir_all(directory);
}

async fn prove_governance_flow(
    ctx: &mut RuntimeContext,
    rx: &mut tokio::sync::mpsc::Receiver<Value>,
    pending: &PendingClientRequests,
) {
    // Exercise the ordinary denial -> native response -> operator resolution
    // flow after setup. Responses below are isolated test-client fixtures.
    let code = "return await temper.create('default', 'AgentTypes', {'id':'setup-flow-probe'})";
    let (denied_result, denials) = ctx.run_execute(code).await;
    assert_eq!(denials.len(), 1);
    let approve_path = format!(
        "{}/api/tenants/default/decisions/{}/approve",
        ctx.base_url, denials[0].decision_id
    );
    let own_approval = ctx
        .http
        .post(approve_path)
        .bearer_auth(ctx.api_key.as_ref().unwrap())
        .json(&json!({"scope":crate::elicit::narrow_scope()}))
        .send()
        .await
        .unwrap();
    assert_eq!(own_approval.status(), reqwest::StatusCode::FORBIDDEN);
    let answer_approval = async {
        let prompt = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            prompt["params"]["message"]
                .as_str()
                .unwrap()
                .contains("pending human approval")
        );
        pending.resolve(json!({"id":prompt["id"],"result":{
            "action":"accept", "content":{"decision":"approve_narrow"}
        }}));
    };
    let (annotated, ()) = tokio::join!(
        crate::elicit::apply_denial_elicitation(ctx, denied_result, denials),
        answer_approval
    );
    assert!(
        annotated
            .unwrap()
            .contains("granted by human via elicitation")
    );
    let (retried, further_denials) = ctx.run_execute(code).await;
    assert!(further_denials.is_empty());
    assert!(retried.unwrap().contains("setup-flow-probe"));
}
