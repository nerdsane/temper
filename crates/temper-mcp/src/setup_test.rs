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
