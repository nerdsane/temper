mod common;

use common::reaction_fixture::*;

const REACTIONS: &str = r#"
[[reaction]]
name = "order_confirmed_authorizes_payment"
[reaction.when]
entity_type = "Order"
action = "ConfirmOrder"
to_state = "Confirmed"
[reaction.then]
entity_type = "Payment"
action = "AuthorizePayment"
[reaction.resolve_target]
type = "same_id"
"#;

#[tokio::test]
async fn awaited_drain_waits_for_logical_retry_and_completes_it() {
    let (_guard, clock, _ids) = install_deterministic_context(421);
    let tenant_name = "shop-retry-421";
    let (state, store) = build_durable_state(tenant_name, REACTIONS);
    let boxed = BoxedEventStore::new(store.clone());
    let rule = parse_reactions(REACTIONS).expect("reaction").remove(0);
    let delivery_id =
        stable_delivery_id(tenant_name, "Order", "o1", "ConfirmOrder", 1, &rule.name, 0);
    let intent = PersistedReactionIntent {
        kind: temper_server::trigger::delivery::DeliveryKind::Reaction,
        delivery_id: delivery_id.clone(),
        root_delivery_id: delivery_id,
        tenant: tenant_name.to_string(),
        source_entity_type: "Order".to_string(),
        source_entity_id: "o1".to_string(),
        source_action: "ConfirmOrder".to_string(),
        source_sequence: 1,
        source_to_state: "Confirmed".to_string(),
        source_fields: serde_json::json!({}),
        guard_passed: true,
        target_entity_id: Some("o1".to_string()),
        trigger_name: rule.name.clone(),
        trigger_index: 0,
        depth: 0,
        rule: serde_json::to_value(rule).expect("serialize rule"),
        authority: serde_json::to_value(
            AgentContext::for_service("retry-test")
                .security_ctx
                .expect("service authority"),
        )
        .expect("serialize authority"),
        created_at: sim_now(),
        not_before: None,
        state_timeout: None,
        schema_pin: None,
    };
    let mut payload = serde_json::json!({});
    attach_intents(&mut payload, std::slice::from_ref(&intent)).expect("attach intent");
    boxed
        .append(
            &format!("{tenant_name}:Order:o1"),
            0,
            &[PersistenceEnvelope {
                sequence_nr: 1,
                event_type: "ConfirmOrder".to_string(),
                payload,
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp: sim_now(),
                    actor_id: format!("{tenant_name}:Order:o1"),
                },
            }],
        )
        .await
        .expect("persist source intent");
    initialize_delivery_record(&boxed, intent.clone())
        .await
        .expect("initialize lifecycle");
    let (mut pending, sequence) = load_delivery_record(&boxed, intent.clone())
        .await
        .expect("load lifecycle");
    pending.next_attempt_at = Some(sim_now() + chrono::Duration::milliseconds(100));
    append_delivery_record(&boxed, sequence, &pending)
        .await
        .expect("persist retry schedule");

    let dispatcher = state
        .reaction_dispatcher
        .read()
        .expect("dispatcher lock")
        .clone()
        .expect("dispatcher");
    let tenant = TenantId::new(tenant_name);
    let (drain_result, ()) = tokio::join!(
        dispatcher.drain_tenant_deliveries(
            &state,
            &tenant,
            1_024,
            std::time::Duration::from_secs(1),
        ),
        async {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            clock.advance_by(2);
        }
    );
    assert_eq!(drain_result.expect("drain retry"), 1);
    let (completed, _) = load_delivery_record(&boxed, intent)
        .await
        .expect("load completed lifecycle");
    assert_eq!(completed.status, ReactionDeliveryStatus::Succeeded);
    assert_eq!(status(&state, &tenant, "Payment", "o1").await, "Authorized");
}
