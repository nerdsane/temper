//! Real atomic sub-writes and a proven core composite parent under spec drift.
use super::*;
use crate::{EntityMsg, EntityResponse, idempotency::ActionReplyMode};
use std::time::Duration;

#[tokio::test]
async fn atomic_child_refuses_but_direct_core_composite_parent_never_emits_new_integration() {
    let (_guard, _, _) = temper_runtime::scheduler::install_deterministic_context(52351);
    let store = SimEventStore::no_faults(52330);
    let warm = composite_test_state_with_store(store.clone());
    let tenant = TenantId::default();
    let actor = warm
        .get_or_spawn_tenant_actor(&tenant, "Parent", "one")
        .unwrap();
    let ask = |key: &str, reason: &str| EntityMsg::Action {
        name: "CreateChild".into(),
        params: json!({"Reason":reason}),
        related: BTreeMap::new(),
        idempotency_key: Some(key.into()),
        expected_authorization_precondition: None,
        reply_mode: ActionReplyMode::DirectCore,
    };
    let original: EntityResponse = actor
        .ask(ask("core", "original"), Duration::from_secs(1))
        .await
        .unwrap();
    assert!(original.success);
    let agent = AgentContext {
        idempotency_key: Some("composite".into()),
        ..AgentContext::for_service("composite-test")
    };
    let writes = json!({"sub_writes":[{"entity_type":"Child","entity_id":"child","action":"Create","params":{"Name":"once"}}]});
    assert!(
        warm.apply_composite_integration_result(
            &tenant,
            "Parent",
            "one",
            "CreateChild",
            &writes,
            &agent
        )
        .await
        .unwrap()
    );
    let child_events = store.dump_journal("default:Child:child");
    let event = &child_events
        .iter()
        .find(|e| e.payload["action"] == "Create" && e.payload["idempotency_key"].is_string())
        .unwrap()
        .payload;
    assert_eq!(event["idempotency_reply"]["version"], 1);
    assert_eq!(event["idempotency_reply"]["reply_class"], "post_completion");
    assert_eq!(event["idempotency_reply"]["mode"]["kind"], "composite");
    assert!(event["idempotency_binding"].is_string());
    assert!(event["idempotency_result"].is_string());
    let child_key = event["idempotency_key"].as_str().unwrap().to_string();
    // Atomic composite append invalidates actor handles; reacquire the real
    // current actor before the unrelated write.
    let actor = warm
        .get_or_spawn_tenant_actor(&tenant, "Parent", "one")
        .unwrap();
    let later: EntityResponse = actor
        .ask(ask("later", "later"), Duration::from_secs(1))
        .await
        .unwrap();
    assert!(later.success);
    assert_eq!(later.state.fields["Reason"], "later");
    drop(warm);
    let cold = composite_test_state_with_store(store.clone());
    let drift=PARENT_IOA.replacen("params = [\"Reason\"]", "params = [\"Reason\"]\n[[action.triggers]]\nname=\"cold_new\"\nkind=\"wasm\"\nmodule=\"cold_new\"",1);
    cold.registry
        .write()
        .unwrap()
        .try_register_tenant_with_constraints(
            "default",
            parse_csdl(COMPOSITE_CSDL).unwrap(),
            COMPOSITE_CSDL.into(),
            &[("Parent", &drift)],
            None,
            true,
        )
        .unwrap();
    let payload = r#"{"success":true,"params":{}}"#;
    let data = payload
        .bytes()
        .map(|b| format!("\\{b:02x}"))
        .collect::<String>();
    let wat = format!(
        r#"(module (import "env" "host_set_result" (func $r (param i32 i32))) (memory (export "memory") 1) (data (i32.const 0) "{data}") (func (export "run") (param i32 i32) (result i32) i32.const 0 i32.const {} call $r i32.const 0))"#,
        payload.len()
    );
    let hash = cold.wasm_engine.compile_and_cache(wat.as_bytes()).unwrap();
    cold.wasm_module_registry
        .write()
        .unwrap()
        .register(&tenant, "cold_new", &hash);
    let child = cold
        .dispatch_tenant_action_ext(
            &tenant,
            "Child",
            "child",
            "Create",
            json!({"Name":"once"}),
            crate::state::DispatchExtOptions {
                agent_ctx: &AgentContext {
                    idempotency_key: Some(child_key),
                    ..AgentContext::for_service("composite-test")
                },
                await_integration: true,
                await_reactions: true,
            },
        )
        .await
        .expect_err("atomic child commit does not certify mandatory projection completion");
    assert!(child.contains("IdempotencyKeyUnverifiable"));
    assert_eq!(
        cold.get_tenant_entity_state(&tenant, "Child", "child")
            .await
            .unwrap()
            .state
            .status,
        "Active"
    );
    let before = serde_json::to_value(store.dump_journal("default:Parent:one")).unwrap();
    let count = store.total_events();
    for _ in 0..2 {
        let reply = cold
            .dispatch_tenant_action_ext(
                &tenant,
                "Parent",
                "one",
                "CreateChild",
                json!({"Reason":"original"}),
                crate::state::DispatchExtOptions {
                    agent_ctx: &AgentContext {
                        idempotency_key: Some("core".into()),
                        ..AgentContext::for_service("composite-test")
                    },
                    await_integration: true,
                    await_reactions: true,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            cold.entity_observe_log
                .lock()
                .unwrap()
                .values()
                .flatten()
                .filter(|e| e.event_name == "integration_complete")
                .count(),
            0,
            "historical composite executed the newly declared real WASM integration"
        );
        assert!(reply.success, "{:?}", reply.error);
        assert_eq!(reply.state.fields, original.state.fields);
        assert!(reply.custom_effects.is_empty());
        assert!(reply.scheduled_actions.is_empty());
        assert!(reply.spawn_requests.is_empty());
        assert!(
            cold.apply_composite_integration_result(
                &tenant,
                "Parent",
                "one",
                "CreateChild",
                &writes,
                &agent
            )
            .await
            .unwrap()
        );
    }
    assert_eq!(
        cold.get_tenant_entity_state(&tenant, "Parent", "one")
            .await
            .unwrap()
            .state
            .fields["Reason"],
        "later"
    );
    assert!(
        !cold
            .entity_observe_log
            .lock()
            .unwrap()
            .values()
            .flatten()
            .any(|e| e.event_name == "integration_complete"),
        "historical composite reran current integration"
    );
    assert_eq!(store.total_events(), count);
    assert_eq!(
        serde_json::to_value(store.dump_journal("default:Parent:one")).unwrap(),
        before
    );
    assert_eq!(
        serde_json::to_value(store.dump_journal("default:Child:child")).unwrap(),
        serde_json::to_value(child_events).unwrap()
    );
}
