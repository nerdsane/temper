//! Setup side-effect boundary exercised through correlated native responses.

use super::*;
use crate::McpConfig;
use crate::client_requests::{ClientRequester, PendingClientRequests};
use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use std::os::unix::fs::PermissionsExt;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

async fn resolve() -> Json<Value> {
    Json(json!({"verified":true, "agent_type_name":"operator", "agent_instance_id":"operator"}))
}

async fn mutation(State(count): State<Arc<AtomicUsize>>) -> StatusCode {
    count.fetch_add(1, Ordering::SeqCst);
    StatusCode::FORBIDDEN
}

async fn fixture() -> (
    RuntimeContext,
    Arc<AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    let count = Arc::new(AtomicUsize::new(0));
    let router = Router::new()
        .route("/api/identity/resolve", post(resolve))
        .fallback(mutation)
        .with_state(count.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let mut ctx = RuntimeContext::from_config(&McpConfig {
        temper_url: Some(format!("http://127.0.0.1:{port}")),
        temper_port: None,
        agent_id: None,
        agent_type: None,
        session_id: None,
        api_key: Some("test-operator".into()),
    })
    .unwrap();
    ctx.approver_key = None;
    ctx.client_supports_elicitation = true;
    ctx.elicit_approvals_enabled = true;
    (ctx, count, server)
}

#[tokio::test]
async fn declined_canceled_malformed_or_disconnected_setup_has_no_side_effects() {
    let (mut ctx, count, server) = fixture().await;
    let directory = std::env::temp_dir().join(format!("temper-consent-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = directory.join("must-not-create.json");
    for result in [
        json!({"action":"decline"}),
        json!({"action":"cancel"}),
        json!({"action":"accept", "content":{"decision":"approve_broad"}}),
        json!({"action":"accept", "content":{"setup":"leave_unchanged"}}),
        json!({"action":"accept", "content":{"setup":"configure_agent_identity","extra":true}}),
        Value::Null,
    ] {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let pending = PendingClientRequests::default();
        ctx.requester = Some(ClientRequester::new(tx, pending.clone()));
        let answer = async {
            let prompt = rx.recv().await.unwrap();
            assert_eq!(prompt["method"], "elicitation/create");
            assert_eq!(count.load(Ordering::SeqCst), 0);
            assert!(!path.exists());
            // A wrong request id cannot authorize this prompt.
            assert!(!pending.resolve(json!({"id":99999,"result":{
                "action":"accept","content":{"setup":"configure_agent_identity"}
            }})));
            if result.is_null() {
                pending.fail_all();
            } else {
                assert!(pending.resolve(json!({"id":prompt["id"],"result":result})));
            }
        };
        let (_, ()) = tokio::join!(setup_at(&mut ctx, &path), answer);
        assert_eq!(count.load(Ordering::SeqCst), 0);
        assert!(!path.exists());
    }
    server.abort();
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn agent_supplied_setup_arguments_are_rejected_before_any_request() {
    let (mut ctx, count, server) = fixture().await;
    let result = setup_connection(&mut ctx, &json!({"approved":true})).await;
    assert!(result.is_err());
    assert_eq!(count.load(Ordering::SeqCst), 0);
    server.abort();
}

#[tokio::test]
async fn setup_notification_cannot_start_administration_or_return_a_response() {
    let (mut ctx, count, server) = fixture().await;
    let result = crate::protocol::dispatch_json_value(
        &mut ctx,
        json!({
            "jsonrpc":"2.0", "method":"tools/call", "params":{"name":"setup_connection"}
        }),
    )
    .await;
    assert!(result.is_none());
    assert_eq!(count.load(Ordering::SeqCst), 0);
    server.abort();
}

#[test]
fn ipv6_loopback_is_an_allowed_local_setup_origin() {
    let url = reqwest::Url::parse("http://[::1]:3600").unwrap();
    assert_eq!(url.host_str(), Some("[::1]"));
    assert!(SetupAdmin::new(url.as_str().trim_end_matches('/'), "default", "test".into()).is_ok());
}

#[tokio::test]
async fn partial_setup_failure_never_restores_operator_execution() {
    use axum::routing::get;
    let count = Arc::new(AtomicUsize::new(0));
    let router = Router::new()
        .route("/api/identity/resolve", post(resolve))
        .route(
            "/api/tenants/default/policies",
            get(|| async { Json(json!({"policy_text":""})) }),
        )
        .route(
            "/api/tenants/default/policies/rules",
            post(|State(count): State<Arc<AtomicUsize>>| async move {
                count.fetch_add(1, Ordering::SeqCst);
                Json(json!({"ok":true}))
            }),
        )
        .fallback(|| async { StatusCode::SERVICE_UNAVAILABLE })
        .with_state(count.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let mut ctx = RuntimeContext::from_config(&McpConfig {
        temper_url: Some(base),
        temper_port: None,
        api_key: Some("test-operator".into()),
        agent_id: Some("operator".into()),
        agent_type: Some("operator".into()),
        session_id: None,
    })
    .unwrap();
    ctx.approver_key = None;
    let directory = std::env::temp_dir().join(format!("temper-partial-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = directory.join("requester.json");
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let pending = PendingClientRequests::default();
    ctx.requester = Some(ClientRequester::new(tx, pending.clone()));
    let answer = async {
        let prompt = rx.recv().await.unwrap();
        assert!(!path.exists());
        pending.resolve(json!({"id":prompt["id"],"result":{
            "action":"accept", "content":{"setup":"configure_agent_identity"}
        }}));
    };
    let (result, ()) = tokio::join!(setup_at(&mut ctx, &path), answer);
    assert!(result.is_err());
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "policy succeeded before provisioning failed"
    );
    let saved = load_identity(&path, &ctx.base_url, &ctx.identity_tenant).unwrap();
    assert_eq!(ctx.api_key.as_deref(), Some(saved.token.as_str()));
    assert_eq!(ctx.approver_key.as_deref(), Some("test-operator"));
    assert_eq!(ctx.agent_id.as_deref(), Some(saved.principal.as_str()));
    let audit = serde_json::to_value(ctx.trajectory.as_ref().unwrap().snapshot()).unwrap();
    assert!(audit.to_string().contains(&saved.principal));
    assert!(!audit.to_string().contains("operator"));
    server.abort();
    std::fs::remove_dir_all(directory).unwrap();
}
