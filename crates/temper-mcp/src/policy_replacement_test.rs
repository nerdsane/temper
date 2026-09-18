//! Native client fixtures are test responses, never live human approvals.
use super::*;
use crate::{
    McpConfig,
    client_requests::{ClientRequester, PendingClientRequests},
};
use axum::{
    Router,
    extract::{Request, State},
    response::IntoResponse,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

const OLD: &str = "forbid(principal, action == Action::\"blocked\", resource);";
const NEW: &str = "forbid(principal, action == Action::\"protected\", resource);";

#[derive(Clone, Default)]
struct Fixture {
    writes: Arc<AtomicUsize>,
}
async fn service(State(state): State<Fixture>, request: Request) -> axum::response::Response {
    let path = request.uri().path().to_owned();
    let key = request
        .headers()
        .get("authorization")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let body = axum::body::to_bytes(request.into_body(), 1024 * 1024)
        .await
        .unwrap();
    if path == "/api/identity/resolve" {
        let requester = key == "Bearer fixture-requester";
        return axum::Json(
            json!({"verified":true,"agent_instance_id":if requester {"requester"} else {"human"}}),
        )
        .into_response();
    }
    if path.ends_with("/list") {
        assert_eq!(key, "Bearer fixture-human");
        let text = if state.writes.load(Ordering::SeqCst) == 0 {
            OLD
        } else {
            NEW
        };
        return axum::Json(json!({"policies":[{"policy_id":"legacy", "cedar_text":text,"policy_hash":digest(text),"enabled":true}]})).into_response();
    }
    assert!(path.ends_with("/legacy/replace"));
    assert_eq!(key, "Bearer fixture-human");
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body, json!({"expected_hash":digest(OLD),"cedar_text":NEW}));
    state.writes.fetch_add(1, Ordering::SeqCst);
    axum::Json(json!({"status":"verified","policy_id":"legacy","tenant":"default","enabled":true,"policy_hash":digest(NEW)})).into_response()
}

#[tokio::test]
async fn native_response_is_the_only_path_to_privileged_write() {
    for response in [
        json!({"action":"accept","content":{"replacement":"apply_exact_replacement"}}),
        json!({"action":"decline","content":{"replacement":"apply_exact_replacement"}}),
        json!({"action":"cancel"}),
        json!({"action":"accept","content":{"decision":"approve_broad"}}),
        json!({"action":"accept","content":{"replacement":"leave_unchanged"}}),
        json!({"action":"accept","content":{"replacement":"apply_exact_replacement","approved":true}}),
        Value::Null,
    ] {
        let state = Fixture::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let router = Router::new().fallback(service).with_state(state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let mut ctx = RuntimeContext::from_config(&McpConfig {
            temper_url: Some(format!("http://127.0.0.1:{port}")),
            temper_port: None,
            api_key: Some("fixture-requester".into()),
            agent_id: None,
            agent_type: None,
            session_id: None,
        })
        .unwrap();
        ctx.approver_key = Some("fixture-human".into());
        ctx.identity_tenant = "default".into();
        ctx.client_supports_elicitation = true;
        ctx.elicit_approvals_enabled = true;
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let pending = PendingClientRequests::default();
        ctx.requester = Some(ClientRequester::new(tx, pending.clone()));
        let call = json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{
            "name":"request_policy_replacement","arguments":{"policy_id":"legacy","expected_hash":digest(OLD),"cedar_text":NEW}}});
        let answer = async {
            let prompt = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap();
            let message = prompt["params"]["message"].as_str().unwrap();
            for disclosed in [OLD, NEW, "legacy", &digest(OLD), &digest(NEW)] {
                assert!(message.contains(disclosed));
            }
            assert!(!message.contains("fixture-human"));
            assert_eq!(state.writes.load(Ordering::SeqCst), 0);
            assert!(!pending.resolve(json!({"id":"wrong-correlation","result":response})));
            if response.is_null() {
                pending.fail_all();
            } else {
                assert!(pending.resolve(json!({"id":prompt["id"],"result":response})));
            }
        };
        let (result, ()) =
            tokio::join!(crate::protocol::dispatch_json_value(&mut ctx, call), answer);
        let exact = response
            == json!({"action":"accept","content":{"replacement":"apply_exact_replacement"}});
        assert_eq!(state.writes.load(Ordering::SeqCst), usize::from(exact));
        assert_eq!(result.unwrap()["result"]["isError"], response.is_null());
        assert_eq!(ctx.api_key.as_deref(), Some("fixture-requester"));
        server.abort();
    }
}

#[tokio::test]
async fn arguments_and_notifications_cannot_supply_consent() {
    let mut ctx = RuntimeContext::from_config(&McpConfig {
        temper_url: Some("http://127.0.0.1:1".into()),
        temper_port: None,
        api_key: None,
        agent_id: None,
        agent_type: None,
        session_id: None,
    })
    .unwrap();
    let mut args = json!({"policy_id":"legacy","expected_hash":digest(OLD),"cedar_text":NEW});
    for name in ["approved", "server", "tenant", "approver_key"] {
        args[name] = json!(true);
        assert!(
            request_policy_replacement(&mut ctx, &args)
                .await
                .unwrap_err()
                .to_string()
                .contains("Invalid replacement")
        );
        args.as_object_mut().unwrap().remove(name);
    }
    let notification = json!({"jsonrpc":"2.0","method":"tools/call","params":{
        "name":"request_policy_replacement","arguments":args}});
    assert!(
        crate::protocol::dispatch_json_value(&mut ctx, notification)
            .await
            .is_none()
    );
}

#[test]
fn identities_and_destinations_must_be_unambiguous() {
    for bad in [
        "https://user:secret@example.test",
        "http://example.test",
        "https://example.test?target=evil",
    ] {
        assert!(PolicyApi::new(bad, "default").is_err());
    }
    assert!(PolicyApi::new("https://example.test", "../other").is_err());
    assert!(
        PolicyApi::new(
            "https://example.test/v1/agent/native/session/temper_platform",
            "default"
        )
        .is_ok()
    );
    assert!(
        require_distinct_identities(
            &json!({"verified":true,"agent_instance_id":"same"}),
            &json!({"verified":true,"agent_instance_id":"same"})
        )
        .is_err()
    );
    assert!(
        require_distinct_identities(&json!({"verified":true}), &json!({"verified":true})).is_err()
    );
}
