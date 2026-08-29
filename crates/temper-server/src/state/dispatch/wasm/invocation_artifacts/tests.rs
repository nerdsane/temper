use super::awaited::callback_agent_context;
use super::persisted_authorization_reason;
use crate::request_context::AgentContext;
use crate::storage::BoxedEventStore;
use crate::trigger::delivery::{
    AwaitedExecutionFailureClass, AwaitedExecutionPhase, DeliveryKind, PersistedReactionIntent,
    ReactionDeliveryRecord, append_delivery_record,
};
use serde_json::json;
use temper_failure::OperationId;

async fn awaited_fixture(
    delivery_id: &str,
    seed: u64,
) -> (
    crate::state::ServerState,
    std::sync::Arc<crate::trigger::dispatcher::AwaitedExecutionOwner>,
    AgentContext,
) {
    let state = crate::state::ServerState::from_registry(
        temper_runtime::ActorSystem::new("typed-awaited-failure"),
        crate::registry::SpecRegistry::new(),
    );
    let store = BoxedEventStore::new(temper_store_sim::SimEventStore::no_faults(seed));
    let now = temper_runtime::scheduler::sim_now();
    let intent = PersistedReactionIntent {
        kind: DeliveryKind::CollectionMember,
        delivery_id: delivery_id.to_string(),
        root_delivery_id: delivery_id.to_string(),
        tenant: "tenant-a".to_string(),
        source_entity_type: "CheckRun".to_string(),
        source_entity_id: "check-1".to_string(),
        source_action: "Start".to_string(),
        source_sequence: 1,
        source_to_state: "Running".to_string(),
        source_fields: json!({}),
        source_stream_descriptor: None,
        guard_passed: true,
        target_entity_id: Some("check-1".to_string()),
        trigger_name: "run-check".to_string(),
        trigger_index: 0,
        depth: 0,
        rule: json!({"name": "run-check"}),
        authority: json!({}),
        created_at: now,
        not_before: None,
        state_timeout: None,
        collection: None,
        schema_pin: None,
    };
    let mut delivery = ReactionDeliveryRecord::pending(intent.clone());
    let fence = delivery.claim(now, chrono::Duration::seconds(30)).unwrap();
    delivery.begin_dispatch(fence).unwrap();
    let sequence = append_delivery_record(&store, 0, &delivery).await.unwrap();
    let owner = crate::trigger::dispatcher::AwaitedExecutionOwner::new(
        store,
        delivery,
        sequence,
        now + chrono::Duration::minutes(1),
    );
    owner
        .bind("run-check", "check.wasm", "abc123", "Succeeded", None, now)
        .await
        .unwrap();
    state.register_awaited_execution_owner(delivery_id, fence, owner.clone());
    let mut owner_ctx = AgentContext {
        idempotency_key: Some(delivery_id.to_string()),
        ..AgentContext::default()
    };
    owner_ctx.observation_metadata.insert(
        crate::state::AWAITED_EXECUTION_FENCE_METADATA.to_string(),
        fence.to_string(),
    );
    (state, owner, owner_ctx)
}

#[test]
fn inline_wasm_callback_has_distinct_stable_idempotency() {
    let parent = AgentContext {
        idempotency_key: Some("member-delivery".to_string()),
        ..AgentContext::default()
    };
    let first = callback_agent_context(
        &parent,
        "validate-task",
        "validate_arc_dataset",
        "RecordValidated",
    );
    let replay = callback_agent_context(
        &parent,
        "validate-task",
        "validate_arc_dataset",
        "RecordValidated",
    );
    let other = callback_agent_context(
        &parent,
        "audit-task",
        "audit_arc_dataset",
        "RecordValidated",
    );
    assert_ne!(first.idempotency_key, parent.idempotency_key);
    assert_eq!(first.idempotency_key, replay.idempotency_key);
    assert_ne!(first.idempotency_key, other.idempotency_key);
}

#[test]
fn typed_authorization_persistence_redacts_raw_diagnostics() {
    let secret_bearing = "denied because token=secret";
    assert_eq!(
        persisted_authorization_reason(secret_bearing, true),
        "AuthorizationDenied"
    );
    assert_eq!(
        persisted_authorization_reason(secret_bearing, false),
        secret_bearing
    );
}

#[tokio::test]
async fn typed_routed_failure_keeps_parent_fence_and_completes_awaited_evidence() {
    let (_guard, _clock, _ids) = temper_runtime::scheduler::install_deterministic_context(8_314);
    let (state, owner, owner_ctx) = awaited_fixture("member-delivery", 8_314).await;
    let typed_ctx = super::super::super::typed_failure::typed_failure_callback_context(
        &owner_ctx,
        &OperationId::new("wasm:typed-failure").unwrap(),
        "TypedFailed",
    );
    assert_ne!(typed_ctx.idempotency_key, owner_ctx.idempotency_key);
    assert!(
        state
            .awaited_execution_owner("member-delivery", &typed_ctx)
            .is_some()
    );
    state
        .complete_awaited_module_failure(
            Some("member-delivery"),
            &owner_ctx,
            Some("TypedFailed"),
            Some(json!({"failure": {"code": "module_failed"}})),
        )
        .await
        .unwrap();
    let evidence = owner.snapshot().await.0.awaited_execution.unwrap();
    assert_eq!(evidence.phase, AwaitedExecutionPhase::ExecutionFailed);
    assert_eq!(evidence.callback_action.as_deref(), Some("TypedFailed"));
    assert_eq!(
        evidence.execution_failure,
        Some(AwaitedExecutionFailureClass::ModuleFailure)
    );
}

#[tokio::test]
async fn unmatched_typed_failure_route_still_terminates_awaited_evidence() {
    let (_guard, _clock, _ids) = temper_runtime::scheduler::install_deterministic_context(8_315);
    let (state, owner, owner_ctx) = awaited_fixture("unmatched-delivery", 8_315).await;
    let error = state
        .settle_awaited_typed_failure(
            Some("unmatched-delivery"),
            &owner_ctx,
            Err("UndeclaredFailureCategory: permanent".to_string()),
            json!({"failure": {"code": "module_failed"}}),
        )
        .await
        .unwrap_err();
    assert!(error.starts_with("UndeclaredFailureCategory:"));
    let evidence = owner.snapshot().await.0.awaited_execution.unwrap();
    assert_eq!(evidence.phase, AwaitedExecutionPhase::ExecutionFailed);
    assert_eq!(evidence.callback_action, None);
    assert_eq!(
        evidence.execution_failure,
        Some(AwaitedExecutionFailureClass::ModuleFailure)
    );
}
