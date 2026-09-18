//! Real HTTP server, persistent store and native JSON-RPC human-response fixtures.
use super::*;
use crate::{
    McpConfig,
    client_requests::{ClientRequester, PendingClientRequests},
};
use std::os::unix::fs::PermissionsExt;
use temper_platform::{
    PlatformState, bootstrap_operator_credential, bootstrap_operator_credential_specs,
};
use temper_server::StorageStack;
use temper_store_turso::TursoEventStore;

#[tokio::test]
async fn native_replacement_commits_and_conflicts_on_real_temper() {
    let directory = std::env::temp_dir().join(format!("replacement-live-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let store = TursoEventStore::new(
        &format!("file:{}", directory.join("test.db").display()),
        None,
    )
    .await
    .unwrap();
    let mut platform = PlatformState::new(None);
    platform
        .server
        .set_storage_stack(StorageStack::from_turso(store.clone()));
    bootstrap_operator_credential_specs(&platform, "default")
        .await
        .unwrap();
    bootstrap_operator_credential(&platform, "fixture-human", "default")
        .await
        .unwrap();
    let state = platform.server.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let router = temper_platform::router::build_platform_router(platform);
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut ctx = RuntimeContext::from_config(&McpConfig {
        temper_url: Some(format!("http://127.0.0.1:{port}")),
        temper_port: None,
        api_key: Some("fixture-human".into()),
        agent_id: None,
        agent_type: None,
        session_id: None,
    })
    .unwrap();
    let (entries, denied) = ctx
        .run_execute("return await temper.get_policy_entries('default')")
        .await;
    assert!(denied.is_empty());
    assert!(entries.unwrap().contains("policies"));
    ctx.approver_key = None;
    ctx.identity_tenant = "default".into();
    ctx.identity_file = Some(directory.join("requester.json"));
    ctx.client_supports_elicitation = true;
    ctx.elicit_approvals_enabled = true;
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let pending = PendingClientRequests::default();
    ctx.requester = Some(ClientRequester::new(tx, pending.clone()));
    let setup_answer = async {
        let prompt = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        pending.resolve(json!({"id":prompt["id"],"result":{"action":"accept","content":{"setup":"configure_agent_identity"}}}));
    };
    let setup_args = json!({});
    let (setup, ()) = tokio::join!(
        crate::setup::setup_connection(&mut ctx, &setup_args),
        setup_answer
    );
    setup.unwrap();
    let api = PolicyApi::new(&ctx.base_url, "default").unwrap();
    // Setup leaves the requester without policy administration, including reads.
    let own_read = ctx
        .http
        .get(format!(
            "{}/api/tenants/default/policies/list",
            ctx.base_url
        ))
        .bearer_auth(ctx.api_key.as_ref().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(own_read.status(), reqwest::StatusCode::FORBIDDEN);
    let old = "forbid(principal, action == Action::\"OldAction\", resource);";
    let new = "forbid(principal, action == Action::\"ProtectedAction\", resource);";
    store
        .save_policy("default", "legacy", old, "fixture")
        .await
        .unwrap();
    temper_server::authz::load_and_activate_tenant_policies(&state, "default").await;
    let args = json!({"policy_id":"legacy","expected_hash":digest(old),"cedar_text":new});
    let before = store.load_policies_for_tenant("default").await.unwrap();
    let answer = async {
        let prompt = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            api.entry("fixture-human", "legacy").await.unwrap()["cedar_text"],
            old
        );
        pending.resolve(json!({"id":prompt["id"],"result":{"action":"accept","content":{"replacement":"apply_exact_replacement"}}}));
    };
    let call = json!({"jsonrpc":"2.0","id":91,"method":"tools/call","params":{"name":"request_policy_replacement","arguments":args}});
    let (result, ()) = tokio::join!(crate::protocol::dispatch_json_value(&mut ctx, call), answer);
    let result = result.unwrap();
    assert_eq!(result["result"]["isError"], false, "{result}");
    assert!(
        state
            .authz
            .get_tenant_policy_text("default")
            .unwrap()
            .contains(new)
    );
    assert!(
        !state
            .authz
            .get_tenant_policy_text("default")
            .unwrap()
            .contains(old)
    );
    let after = store.load_policies_for_tenant("default").await.unwrap();
    for row in before.iter().filter(|row| row.policy_id != "legacy") {
        assert_eq!(
            after
                .iter()
                .find(|next| next.policy_id == row.policy_id)
                .unwrap()
                .cedar_text,
            row.cedar_text
        );
    }
    // Stale approval must fail in the actual SQL operation, even if it was accepted earlier.
    let stale = ctx
        .http
        .post(format!(
            "{}/api/tenants/default/policies/entry/legacy/replace",
            ctx.base_url
        ))
        .bearer_auth("fixture-human")
        .json(&json!({"expected_hash":digest(old),"cedar_text":old}))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), reqwest::StatusCode::CONFLICT);
    let denied = ctx
        .http
        .post(format!(
            "{}/api/tenants/default/policies/entry/legacy/replace",
            ctx.base_url
        ))
        .bearer_auth(ctx.api_key.as_ref().unwrap())
        .json(&json!({"expected_hash":digest(new),"cedar_text":old}))
        .send()
        .await
        .unwrap();
    assert!(!denied.status().is_success());
    assert_eq!(
        api.entry("fixture-human", "legacy").await.unwrap()["cedar_text"],
        new
    );
    server.abort();
    drop(store);
    let _ = std::fs::remove_dir_all(directory);
}
