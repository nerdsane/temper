use super::*;
use temper_runtime::persistence::EventStore;

#[tokio::test]
async fn existing_child_initializer_compares_logical_blob_values() {
    let parent = r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted"]
initial = "Draft"
[[action]]
name = "SpawnChild"
from = ["Draft"]
to = "Submitted"
params = ["child_id", "expected"]
effect = ["spawn('Customer', 'Initialize', child_ref, params.child_id)"]
"#;
    let child = r#"
[automaton]
name = "Customer"
states = ["Draft", "Ready"]
initial = "Draft"
strict_action_params = true
[[state]]
name = "Name"
type = "string"
initial = ""
[[action]]
name = "Write"
from = ["Draft"]
params = ["Name"]
[[action]]
name = "Initialize"
from = ["Draft"]
to = "Ready"
params = ["expected"]
[[action.constraints]]
kind = "param_equals_field"
param = "expected"
field = "Name"
"#;
    let dir = tempfile::tempdir().unwrap();
    let store = temper_store_turso::TursoEventStore::new(
        dir.path().join("events.db").to_str().unwrap(),
        None,
    )
    .await
    .unwrap();
    let (mut state, _) = common::build_single_tenant_state(
        0,
        "existing-child",
        "default",
        &[("Order", parent), ("Customer", child)],
    );
    state.data_dir = dir.path().to_path_buf();
    state.set_storage_stack(temper_server::StorageStack::from_turso(store));
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .unwrap();
    let tenant = TenantId::default();
    let large = "N".repeat(512 * 1024);
    let written = state
        .dispatch_tenant_action(
            &tenant,
            "Customer",
            "child",
            "Write",
            json!({"Name":large}),
            &Default::default(),
        )
        .await
        .unwrap();
    assert!(written.success);
    let descriptor = written.state.fields["Name"].clone();
    assert!(descriptor.is_object(), "fixture did not overflow to a blob");
    state
        .get_or_create_tenant_entity(&tenant, "Order", "parent", json!({}))
        .await
        .unwrap();
    let mut events = state.entity_observe_tx.subscribe();
    state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "parent",
            "SpawnChild",
            json!({"child_id":"child", "expected":large}),
            &Default::default(),
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        for _ in 0..32 {
            let event = events.recv().await.unwrap();
            assert_ne!(
                event.event_name, "integration_callback_rejected",
                "{event:?}"
            );
            if event.entity_id == "child" && event.data["action"] == "Initialize" {
                return;
            }
        }
        panic!("existing child initializer never completed");
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn invalid_absent_action_contracts_leave_no_entity_or_persistence() {
    check_invalid_absent_action(false).await;
}

#[tokio::test]
async fn invalid_absent_public_dispatch_leaves_no_entity_or_persistence() {
    check_invalid_absent_action(true).await;
}

async fn submit_absent(state: &ServerState, body: serde_json::Value, direct: bool) -> StatusCode {
    if direct {
        let response = state
            .dispatch_tenant_action(
                &TenantId::default(),
                "Order",
                "absent",
                "SubmitOrder",
                body,
                &Default::default(),
            )
            .await
            .unwrap();
        if response.success {
            StatusCode::OK
        } else {
            StatusCode::CONFLICT
        }
    } else {
        request(
            state,
            "POST",
            "/tdata/Orders('absent')/Temper.SubmitOrder",
            body,
        )
        .await
    }
}

async fn check_invalid_absent_action(direct: bool) {
    let spec = SPEC
        .replace(
            "params = [\"Notes\"]",
            r#"params = ["Notes", "expected"]
[[action.constraints]]
kind = "param_nonempty"
param = "Notes"
[[action.constraints]]
kind = "param_equals_field"
param = "expected"
field = "revision"
"#,
        )
        .replace(
            "[[action]]",
            r#"[[state]]
name = "revision"
type = "counter"
initial = "7"
[[action]]"#,
        );
    for (index, body) in [
        json!({"Notes":"valid","expected":7,"extra":true}),
        json!({"Notes":"valid"}),
        json!({"Notes":"","expected":7}),
        json!({"Notes":"valid","expected":6}),
    ]
    .into_iter()
    .enumerate()
    {
        let store = temper_store_sim::SimEventStore::no_faults(467 + index as u64);
        let mut state = state_with_spec(common::CSDL_XML, &spec);
        let dir = tempfile::tempdir().unwrap();
        let audit = temper_store_turso::TursoEventStore::new(
            dir.path().join("audit.db").to_str().unwrap(),
            None,
        )
        .await
        .unwrap();
        let audit_stack = temper_server::StorageStack::from_turso(audit);
        let mut stack = temper_server::StorageStack::from_sim(store.clone(), None);
        stack.trajectory = audit_stack.trajectory;
        stack.metadata = audit_stack.metadata;
        state.set_storage_stack(stack);
        assert_eq!(
            submit_absent(&state, body, direct).await,
            StatusCode::CONFLICT
        );
        assert_eq!(
            state.active_actor_count(),
            0,
            "invalid input materialized an actor"
        );
        assert!(!state.entity_exists(&TenantId::default(), "Order", "absent"));
        assert!(
            store
                .read_events("default:Order:absent", 0)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .load_snapshot("default:Order:absent")
                .await
                .unwrap()
                .is_none()
        );
        if direct {
            assert_eq!(
                state
                    .metrics
                    .errors_total
                    .load(std::sync::atomic::Ordering::Relaxed),
                1
            );
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    let entries = state.load_trajectory_entries("default", 10).await;
                    if !entries.is_empty() {
                        assert_eq!(entries.len(), 1);
                        assert_eq!(entries[0].action, "SubmitOrder");
                        assert!(!entries[0].success);
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        }
        assert_eq!(
            submit_absent(&state, json!({"Notes":"valid","expected":7}), direct).await,
            StatusCode::OK
        );
        assert_eq!(state.active_actor_count(), 1);
    }
}

#[tokio::test]
async fn rejected_child_initializer_leaves_no_child_or_persistence() {
    check_child_initializer(false, "").await;
}

#[tokio::test]
async fn static_child_initializer_obeys_its_contract() {
    check_child_initializer(true, "").await;
    check_child_initializer(true, "valid").await;
}

async fn check_child_initializer(use_static: bool, payload: &str) {
    let parent = r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted"]
initial = "Draft"
[[action]]
name = "SpawnChild"
from = ["Draft"]
to = "Submitted"
params = ["child_id", "payload"]
effect = ["spawn('Customer', 'Initialize', child_ref, params.child_id)"]
"#;
    let child = r#"
[automaton]
name = "Customer"
states = ["Draft", "Ready"]
initial = "Draft"
strict_action_params = true
[[action]]
name = "Initialize"
from = ["Draft"]
to = "Ready"
params = ["payload"]
[[action.constraints]]
kind = "param_nonempty"
param = "payload"
"#;
    let store = temper_store_sim::SimEventStore::no_faults(467);
    let state = if use_static {
        ServerState::with_storage_stack(
            ActorSystem::new("static-child"),
            parse_csdl(common::CSDL_XML).unwrap(),
            common::CSDL_XML.into(),
            std::collections::BTreeMap::from([
                ("Order".into(), parent.into()),
                ("Customer".into(), child.into()),
            ]),
            temper_server::StorageStack::from_sim(store.clone(), None),
        )
        .unwrap()
    } else {
        let (mut state, _) = common::build_single_tenant_state(
            0,
            "refused-child",
            "default",
            &[("Order", parent), ("Customer", child)],
        );
        state.set_storage_stack(temper_server::StorageStack::from_sim(store.clone(), None));
        state
    };
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .unwrap();
    let tenant = TenantId::default();
    state
        .get_or_create_tenant_entity(&tenant, "Order", "parent", json!({}))
        .await
        .unwrap();
    let mut events = state.entity_observe_tx.subscribe();
    state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "parent",
            "SpawnChild",
            json!({"child_id":"child","payload":payload}),
            &Default::default(),
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        for _ in 0..32 {
            let event = events.recv().await.unwrap();
            if !payload.is_empty()
                && event.entity_id == "child"
                && event.data["action"] == "Initialize"
            {
                return;
            }
            if event.entity_id == "parent" && event.event_name == "integration_callback_rejected" {
                assert!(payload.is_empty(), "valid initializer refused: {event:?}");
                assert_eq!(event.data["action"], "Initialize");
                return;
            }
        }
        panic!("initializer refusal was not observable");
    })
    .await
    .unwrap();
    if !payload.is_empty() {
        assert!(state.entity_exists(&tenant, "Customer", "child"));
        return;
    }
    assert_eq!(
        state.active_actor_count(),
        1,
        "rejected initializer created a child"
    );
    assert!(!state.entity_exists(&tenant, "Customer", "child"));
    assert!(
        store
            .read_events("default:Customer:child", 0)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .load_snapshot("default:Customer:child")
            .await
            .unwrap()
            .is_none()
    );
}
