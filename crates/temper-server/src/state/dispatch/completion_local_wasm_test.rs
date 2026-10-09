//! Real engine -> host_http_call -> local OData -> credentialed owned child.
use super::tests::fixture::*;
use crate::registry::{EntityVerificationResult, VerificationStatus};
use crate::request_context::AgentContext;
use crate::state::DispatchExtOptions;
use serde_json::json;
use std::sync::atomic::Ordering;
use std::time::Duration;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;

pub(super) const CSDL: &str = r#"<?xml version="1.0"?><edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx"><edmx:DataServices><Schema Namespace="Temper" xmlns="http://docs.oasis-open.org/odata/ns/edm"><EntityType Name="Work"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType><Action Name="Start" IsBound="true"><Parameter Name="bindingParameter" Type="Temper.Work"/></Action><EntityContainer Name="Local"><EntitySet Name="Works" EntityType="Temper.Work"/></EntityContainer></Schema></edmx:DataServices></edmx:Edmx>"#;
pub(super) const PARENT: &str = r#"
[automaton]
name="Parent"
states=["Idle","Running","Done","Failed"]
initial="Idle"
[[action]]
name="Start"
from=["Idle"]
to="Running"
[[action.triggers]]
name="local"
kind="wasm"
module="local"
on_success="Complete"
on_failure="Fail"
[action.triggers.config]
timeout_secs="1"
[[action]]
name="Complete"
from=["Running"]
to="Done"
[[action]]
name="Fail"
from=["Running"]
to="Failed"
"#;

fn guest(success: bool, timeout: bool) -> String {
    let url = "http://localhost:3000/tdata/Works('one')/Temper.Start?await_integration=true";
    let payload = json!({"success":success,"params":{},"error":if success { "" } else { "ordinary guest failure after local timeout" }}).to_string();
    let encode = |bytes: &[u8]| {
        bytes
            .iter()
            .map(|b| format!("\\{b:02x}"))
            .collect::<String>()
    };
    let length = encode(&(payload.len() as u32).to_le_bytes());
    let data = encode(payload.as_bytes());
    let comparison = if timeout {
        "i32.const -1 i32.eq"
    } else {
        "i32.const 0 i32.gt_s"
    };
    format!(
        r#"(module
        (import "env" "host_http_call" (func $http (param i32 i32 i32 i32 i32 i32 i32 i32 i32 i32) (result i32)))
        (memory (export "memory") 1)
        (data (i32.const 0) "POST")
        (data (i32.const 64) "{url}")
        (data (i32.const 1024) "{{}}")
        (data (i32.const 2044) "{length}{data}")
        (func (export "run") (param i32 i32) (result i32)
            i32.const 0 i32.const 4 i32.const 64 i32.const {url_len}
            i32.const 0 i32.const 0 i32.const 1024 i32.const 2
            i32.const 4096 i32.const 8192 call $http
            {comparison}
            if (result i32) i32.const 2048 else unreachable end))"#,
        url_len = url.len()
    )
}

pub(super) fn completed_invocations(state: &crate::ServerState) -> Vec<serde_json::Value> {
    state
        .entity_observe_log
        .lock()
        .unwrap()
        .values()
        .flatten()
        .filter(|e| e.entity_type == "Parent" && e.event_name == "integration_complete")
        .map(|e| e.data.clone())
        .collect()
}

async fn local_child_outlives_host_wait(guest_success: bool, quiescent: bool) {
    let (state, store, adapter) = credential_fixture().await;
    state
        .registry
        .write()
        .unwrap()
        .try_register_tenant_with_constraints(
            "default",
            parse_csdl(CSDL).unwrap(),
            CSDL.into(),
            &[("Parent", PARENT)],
            None,
            true,
        )
        .unwrap();
    state.registry.write().unwrap().set_verification_status(
        &TenantId::default(),
        "Work",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![],
            verified_at: "test".into(),
        }),
    );
    state.rebuild_reaction_dispatcher();
    let hash = state
        .wasm_engine
        .compile_and_cache(guest(guest_success, !quiescent).as_bytes())
        .unwrap();
    state
        .wasm_module_registry
        .write()
        .unwrap()
        .register(&TenantId::default(), "local", &hash);
    if quiescent {
        adapter.finish.add_permits(1);
    }
    let invoke = |key| async {
        state
            .dispatch_tenant_action_ext(
                &TenantId::default(),
                "Parent",
                "one",
                "Start",
                json!({}),
                DispatchExtOptions {
                    agent_ctx: &AgentContext {
                        idempotency_key: key,
                        ..AgentContext::system()
                    },
                    await_integration: true,
                    await_reactions: true,
                },
            )
            .await
            .unwrap()
    };
    let mut parent = Box::pin(invoke(None));
    let joined_response = if quiescent {
        // This child is already released, so completion may precede observing
        // its started notification. Assert the joined outcome, not poll order.
        Some(
            tokio::time::timeout(Duration::from_secs(5), &mut parent)
                .await
                .unwrap(),
        )
    } else {
        tokio::select! { biased;
            response = &mut parent => panic!("parent returned before local child execution: {response:?}"),
            _ = adapter.started.notified() => {},
            _ = tokio::time::sleep(Duration::from_secs(5)) => panic!("local child never started")
        }
        None
    };
    let token = adapter.hashes.lock().unwrap()[0].clone();
    let parent_key = store
        .dump_journal("default:Parent:one")
        .into_iter()
        .find(|e| e.payload["action"] == "Start")
        .unwrap()
        .payload["idempotency_key"]
        .as_str()
        .unwrap()
        .to_owned();
    let response = match joined_response {
        Some(response) => response,
        None => tokio::time::timeout(Duration::from_secs(5), parent)
            .await
            .unwrap(),
    };
    let invocations = completed_invocations(&state);
    assert_eq!(invocations.len(), 1);
    assert_eq!(
        invocations[0]["result"],
        if guest_success { "success" } else { "failure" },
        "guest did not deliver the intended ordinary result: {invocations:?}"
    );
    if quiescent {
        assert!(response.success, "{response:?}");
        assert_eq!(
            action_count(&store, "Parent", "Complete"),
            1,
            "joined success lost its callback"
        );
        assert_eq!(credential_status(&state, &token).await, "Revoked");
        assert_eq!(
            state
                .idempotency_cache
                .continuation_slots
                .available_permits(),
            64
        );
    } else {
        assert!(
            !response.success,
            "guest success concealed a surviving local child"
        );
        assert!(
            response
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("completion is unknown"),
            "parent falsely certified a joined failure: {response:?}"
        );
        assert_eq!(
            action_count(&store, "Parent", "Complete"),
            0,
            "uncertain guest success dispatched its result callback"
        );
        assert_eq!(
            action_count(&store, "Parent", "Fail"),
            0,
            "uncertain guest failure dispatched fallback/compensation"
        );
        assert_eq!(invocations[0]["operation_completion"], "unknown");
        assert_eq!(credential_status(&state, &token).await, "Active");
        assert!(!invoke(Some(parent_key.clone())).await.success);
        assert_eq!(
            completed_invocations(&state).len(),
            1,
            "retry invoked the parent again while its local child survived"
        );
        assert_eq!(
            state
                .idempotency_cache
                .continuation_slots
                .available_permits(),
            63,
            "parent/child did not share their root lease"
        );
        adapter.finish.add_permits(1);
        tokio::time::timeout(Duration::from_secs(3), async {
            while credential_status(&state, &token).await != "Revoked" {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!invoke(Some(parent_key.clone())).await.success);
        assert_eq!(
            state
                .idempotency_cache
                .continuation_slots
                .available_permits(),
            63
        );
    }
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
    assert_eq!(action_count(&store, "Work", "Start"), 1);
    assert_eq!(
        action_count_id(&store, "AgentCredential", &token, "Revoke"),
        1
    );
    let child_key = store
        .dump_journal("default:Work:one")
        .into_iter()
        .find(|e| e.payload["action"] == "Start")
        .unwrap()
        .payload["idempotency_key"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(
        child_key, parent_key,
        "parent key was copied to another action binding"
    );
}

#[tokio::test]
async fn ordinary_guest_error_cannot_release_surviving_local_child() {
    local_child_outlives_host_wait(false, false).await;
}
#[tokio::test]
async fn handled_host_timeout_success_cannot_certify_local_child_completion() {
    local_child_outlives_host_wait(true, false).await;
}
#[tokio::test]
async fn normally_joined_local_child_keeps_normal_success() {
    local_child_outlives_host_wait(true, true).await;
}
