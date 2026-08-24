mod common;

use common::reaction_fixture::*;

#[tokio::test]
async fn restart_after_target_commit_reconciles_without_duplicate_target_event() {
    let (_guard, _clock, _ids) = install_deterministic_context(415);
    let reactions = r#"
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
    let tenant_name = "shop-restart-415";
    let (state, store) = build_durable_state(tenant_name, reactions);
    let tenant = TenantId::new(tenant_name);
    dispatch(
        &state,
        &tenant,
        "Order",
        "o1",
        "AddItem",
        serde_json::json!({}),
    )
    .await;
    dispatch(
        &state,
        &tenant,
        "Order",
        "o1",
        "SubmitOrder",
        serde_json::json!({}),
    )
    .await;
    dispatch(
        &state,
        &tenant,
        "Order",
        "o1",
        "ConfirmOrder",
        serde_json::json!({}),
    )
    .await;

    let source = store.dump_journal(&format!("{tenant_name}:Order:o1"));
    let intent = extract_intents(
        &source
            .iter()
            .find(|event| event.event_type == "ConfirmOrder")
            .expect("source event must exist")
            .payload,
    )
    .expect("source intent must decode")
    .pop()
    .expect("source intent must exist");
    let lifecycle_id = delivery_journal_id(&intent);
    let lifecycle = store.dump_journal(&lifecycle_id);
    let mut ambiguous: ReactionDeliveryRecord = serde_json::from_value(
        lifecycle
            .last()
            .expect("lifecycle must exist")
            .payload
            .clone(),
    )
    .expect("lifecycle must decode");
    ambiguous.status = ReactionDeliveryStatus::Dispatching;
    ambiguous.lease_expires_at = Some(sim_now() - chrono::Duration::seconds(1));
    append_delivery_record(
        &temper_server::storage::BoxedEventStore::new(store.clone()),
        lifecycle
            .last()
            .expect("lifecycle sequence must exist")
            .sequence_nr,
        &ambiguous,
    )
    .await
    .expect("ambiguous crash state must persist");
    drop(state);

    let mut restarted = build_state(tenant_name, reactions);
    restarted.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    let dispatcher = restarted
        .reaction_dispatcher
        .read()
        .expect("dispatcher lock")
        .clone()
        .expect("dispatcher");
    dispatcher
        .dispatch_committed_intent(&restarted, intent)
        .await
        .expect("expired delivery should recover");

    let target = store.dump_journal(&format!("{tenant_name}:Payment:o1"));
    assert_eq!(
        target
            .iter()
            .filter(|event| event.event_type == "AuthorizePayment")
            .count(),
        1,
        "target idempotency identity must suppress duplicate commit after restart"
    );
    let recovered = store.dump_journal(&lifecycle_id);
    let latest: ReactionDeliveryRecord = serde_json::from_value(
        recovered
            .last()
            .expect("recovered lifecycle")
            .payload
            .clone(),
    )
    .expect("recovered lifecycle must decode");
    assert_eq!(latest.status, ReactionDeliveryStatus::Succeeded);
    assert_eq!(
        latest.fencing_token, ambiguous.fencing_token,
        "receipt reconciliation must finish without another target attempt"
    );
}

#[tokio::test]
async fn recovery_scan_uses_persisted_rule_after_current_rules_are_removed() {
    let (_guard, _clock, _ids) = install_deterministic_context(416);
    let reactions = r#"
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
    let tenant_name = "shop-recovery-416";
    let store = SimEventStore::no_faults(416);
    let boxed = BoxedEventStore::new(store.clone());
    let rule = parse_reactions(reactions)
        .expect("reaction must parse")
        .pop()
        .expect("reaction must exist");
    let delivery_id =
        stable_delivery_id(tenant_name, "Order", "o1", "ConfirmOrder", 1, &rule.name, 0);
    let authority = AgentContext::for_service("recovery-test")
        .security_ctx
        .expect("service authority");
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
        rule: serde_json::to_value(rule).expect("rule must serialize"),
        authority: serde_json::to_value(authority).expect("authority must serialize"),
        created_at: sim_now(),
        not_before: None,
        state_timeout: None,
        schema_pin: None,
    };
    let mut payload = serde_json::json!({
        "action": "ConfirmOrder",
        "from_status": "Submitted",
        "to_status": "Confirmed",
        "timestamp": sim_now(),
        "params": {},
        "idempotency_key": "source-416"
    });
    attach_intents(&mut payload, std::slice::from_ref(&intent)).expect("intent must attach");
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
        .expect("source event and intent must persist");
    store.fail_next_reads(&format!("{tenant_name}:Order:o1"), 1);

    let mut restarted = build_state_without_storage(tenant_name, "");
    restarted.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if !store.dump_journal(&delivery_journal_id(&intent)).is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("persistent recovery must retry after the injected startup read failure");
    assert_eq!(
        status(&restarted, &TenantId::new(tenant_name), "Payment", "o1").await,
        "Authorized"
    );
    let lifecycle = store.dump_journal(&delivery_journal_id(&intent));
    let latest: ReactionDeliveryRecord = serde_json::from_value(
        lifecycle
            .last()
            .expect("lifecycle must exist")
            .payload
            .clone(),
    )
    .expect("lifecycle must decode");
    assert_eq!(latest.status, ReactionDeliveryStatus::Succeeded);
}

#[tokio::test]
async fn startup_recovery_reaches_delivery_beyond_ten_thousand_source_journals() {
    let (_guard, _clock, _ids) = install_deterministic_context(419);
    let reactions = r#"
[[reaction]]
name = "late_delivery"
[reaction.when]
entity_type = "Order"
action = "ConfirmOrder"
[reaction.then]
entity_type = "Payment"
action = "AuthorizePayment"
[reaction.resolve_target]
type = "same_id"
"#;
    let tenant_name = "shop-starvation-419";
    let store = SimEventStore::no_faults(419);
    let boxed = BoxedEventStore::new(store.clone());
    for index in 0..10_001 {
        let entity_id = format!("a{index:05}");
        boxed
            .append(
                &format!("{tenant_name}:Order:{entity_id}"),
                0,
                &[PersistenceEnvelope {
                    sequence_nr: 1,
                    event_type: "Seed".to_string(),
                    payload: serde_json::json!({}),
                    metadata: EventMetadata {
                        event_id: sim_uuid(),
                        causation_id: sim_uuid(),
                        correlation_id: sim_uuid(),
                        timestamp: sim_now(),
                        actor_id: entity_id,
                    },
                }],
            )
            .await
            .expect("seed source journal");
    }

    let rule = parse_reactions(reactions).expect("reaction").remove(0);
    let delivery_id = stable_delivery_id(
        tenant_name,
        "Order",
        "zzzz",
        "ConfirmOrder",
        1,
        &rule.name,
        0,
    );
    let intent = PersistedReactionIntent {
        kind: temper_server::trigger::delivery::DeliveryKind::Reaction,
        delivery_id: delivery_id.clone(),
        root_delivery_id: delivery_id,
        tenant: tenant_name.to_string(),
        source_entity_type: "Order".to_string(),
        source_entity_id: "zzzz".to_string(),
        source_action: "ConfirmOrder".to_string(),
        source_sequence: 1,
        source_to_state: "Confirmed".to_string(),
        source_fields: serde_json::json!({}),
        guard_passed: true,
        target_entity_id: Some("zzzz".to_string()),
        trigger_name: rule.name.clone(),
        trigger_index: 0,
        depth: 0,
        rule: serde_json::to_value(rule).expect("serialize rule"),
        authority: serde_json::to_value(
            AgentContext::for_service("starvation-test")
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
            &format!("{tenant_name}:Order:zzzz"),
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
                    actor_id: "zzzz".to_string(),
                },
            }],
        )
        .await
        .expect("persist late intent");

    let mut restarted = build_state_without_storage(tenant_name, reactions);
    restarted.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if !store.dump_journal(&delivery_journal_id(&intent)).is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("keyset recovery must reach the late journal");
    assert_eq!(
        status(&restarted, &TenantId::new(tenant_name), "Payment", "zzzz").await,
        "Authorized"
    );
}

#[tokio::test]
async fn delivery_uses_committed_cross_entity_guard_decision_after_target_changes() {
    let reactions = r#"
[[reaction]]
name = "guarded_delivery"
[reaction.when]
entity_type = "Order"
action = "ConfirmOrder"
[reaction.when.guard]
type = "cross_entity_state_in"
entity_type = "Payment"
entity_id_source = "guard_id"
required_status = ["Pending"]
[reaction.then]
entity_type = "Payment"
action = "AuthorizePayment"
[reaction.resolve_target]
type = "same_id"
"#;
    let tenant_name = "guard-snapshot-420";
    let (state, _) = build_durable_state(tenant_name, reactions);
    let tenant = TenantId::new(tenant_name);
    dispatch(
        &state,
        &tenant,
        "Payment",
        "guard",
        "FailPayment",
        serde_json::json!({}),
    )
    .await;

    let rule = parse_reactions(reactions).expect("reaction").remove(0);
    let delivery_id = stable_delivery_id(
        tenant_name,
        "Order",
        "target",
        "ConfirmOrder",
        1,
        &rule.name,
        0,
    );
    let intent = PersistedReactionIntent {
        kind: temper_server::trigger::delivery::DeliveryKind::Reaction,
        delivery_id: delivery_id.clone(),
        root_delivery_id: delivery_id,
        tenant: tenant_name.to_string(),
        source_entity_type: "Order".to_string(),
        source_entity_id: "target".to_string(),
        source_action: "ConfirmOrder".to_string(),
        source_sequence: 1,
        source_to_state: "Confirmed".to_string(),
        source_fields: serde_json::json!({"guard_id": "guard"}),
        guard_passed: true,
        target_entity_id: Some("target".to_string()),
        trigger_name: rule.name.clone(),
        trigger_index: 0,
        depth: 0,
        rule: serde_json::to_value(rule).expect("serialize rule"),
        authority: serde_json::to_value(
            AgentContext::for_service("guard-snapshot-test")
                .security_ctx
                .expect("service authority"),
        )
        .expect("serialize authority"),
        created_at: sim_now(),
        not_before: None,
        state_timeout: None,
        schema_pin: None,
    };
    let dispatcher = state
        .reaction_dispatcher
        .read()
        .expect("dispatcher lock")
        .clone()
        .expect("dispatcher");
    dispatcher
        .dispatch_committed_intent(&state, intent)
        .await
        .expect("committed guard decision must dispatch");
    assert_eq!(
        status(&state, &tenant, "Payment", "target").await,
        "Authorized"
    );
}

// =========================================================================
// E2E-2: Guarded reaction — source field gate.
//
// Two rules on the same trigger; a source-field guard picks exactly one.
// Proves ReactionGuard evaluation works through the production path.
// =========================================================================
