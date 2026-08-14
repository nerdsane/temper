mod common;

use common::reaction_fixture::*;
use temper_server::state::DispatchExtOptions;

const TWO_STEP_REACTIONS: &str = r#"
[[reaction]]
name = "order_confirmed_authorizes_payment"
[reaction.when]
entity_type = "Order"
action = "ConfirmOrder"
[reaction.then]
entity_type = "Payment"
action = "AuthorizePayment"
[reaction.resolve_target]
type = "same_id"

[[reaction]]
name = "payment_authorized_captures_payment"
[reaction.when]
entity_type = "Payment"
action = "AuthorizePayment"
[reaction.then]
entity_type = "Payment"
action = "CapturePayment"
[reaction.resolve_target]
type = "same_id"
"#;

#[tokio::test]
async fn mandatory_reaction_fails_closed_without_event_journal() {
    let reactions = r#"
[[reaction]]
name = "required_delivery"
[reaction.when]
entity_type = "Order"
action = "ConfirmOrder"
[reaction.then]
entity_type = "Payment"
action = "AuthorizePayment"
[reaction.resolve_target]
type = "same_id"
"#;
    let state = build_state_without_storage("fail-closed", reactions);
    let error = state
        .dispatch_tenant_action(
            &TenantId::new("fail-closed"),
            "Order",
            "o1",
            "ConfirmOrder",
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await
        .expect_err("required durable reaction must reject a volatile source write");
    assert!(
        error
            .to_string()
            .contains("durable reactions require a configured event journal")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_awaiting_dispatch_returns_after_durability_when_workers_are_saturated() {
    let reactions = r#"
[[reaction]]
name = "required_delivery"
[reaction.when]
entity_type = "Order"
action = "ConfirmOrder"
[reaction.then]
entity_type = "Payment"
action = "AuthorizePayment"
[reaction.resolve_target]
type = "same_id"
"#;
    let tenant_name = "non-await-saturation";
    let (state, store) = build_durable_state(tenant_name, reactions);
    let tenant = TenantId::new(tenant_name);
    for index in 0..11 {
        let entity_id = format!("o{index}");
        dispatch(
            &state,
            &tenant,
            "Order",
            &entity_id,
            "AddItem",
            serde_json::json!({}),
        )
        .await;
        dispatch(
            &state,
            &tenant,
            "Order",
            &entity_id,
            "SubmitOrder",
            serde_json::json!({}),
        )
        .await;
        store.inject_append_delay(
            &format!("{tenant_name}:Payment:{entity_id}"),
            std::time::Duration::from_secs(2),
        );
    }

    let context = AgentContext::default();
    let mut tasks = Vec::new();
    for index in 0..10 {
        let state = state.clone();
        let tenant = tenant.clone();
        let context = context.clone();
        tasks.push(tokio::spawn(async move {
            state
                .dispatch_tenant_action_ext(
                    &tenant,
                    "Order",
                    &format!("o{index}"),
                    "ConfirmOrder",
                    serde_json::json!({}),
                    DispatchExtOptions {
                        agent_ctx: &context,
                        await_integration: false,
                        await_reactions: false,
                    },
                )
                .await
                .expect("source commit")
        }));
    }
    for task in tasks {
        task.await.expect("source task");
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    tokio::time::timeout(
        std::time::Duration::from_millis(500),
        state.dispatch_tenant_action_ext(
            &tenant,
            "Order",
            "o10",
            "ConfirmOrder",
            serde_json::json!({}),
            DispatchExtOptions {
                agent_ctx: &context,
                await_integration: false,
                await_reactions: false,
            },
        ),
    )
    .await
    .expect("non-awaiting source must not wait for a worker permit")
    .expect("source commit must succeed");

    let source = store.dump_journal(&format!("{tenant_name}:Order:o10"));
    let intent = source
        .iter()
        .find(|event| event.event_type == "ConfirmOrder")
        .and_then(|event| extract_intents(&event.payload).ok())
        .and_then(|mut intents| intents.pop())
        .expect("source intent must be committed");
    let visible = find_delivery_record(
        &BoxedEventStore::new(store),
        tenant_name,
        &intent.delivery_id,
    )
    .await
    .expect("read delivery record")
    .expect("committed non-awaited delivery must be immediately observable");
    assert_eq!(visible.0.status, ReactionDeliveryStatus::Pending);
}

#[tokio::test]
async fn durable_dispatch_records_source_intent_target_receipt_and_success() {
    let (_guard, _clock, _ids) = install_deterministic_context(414);
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
    let tenant_name = "shop-durable-414";
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
    let source_event = source
        .iter()
        .find(|event| event.event_type == "ConfirmOrder")
        .expect("source event must be durable");
    let intents = extract_intents(&source_event.payload).expect("source intent must decode");
    assert_eq!(intents.len(), 1);

    let lifecycle = store.dump_journal(&delivery_journal_id(&intents[0]));
    let record: ReactionDeliveryRecord = serde_json::from_value(
        lifecycle
            .last()
            .expect("delivery lifecycle must be durable")
            .payload
            .clone(),
    )
    .expect("delivery record must decode");
    assert_eq!(record.status, ReactionDeliveryStatus::Succeeded);

    let target = store.dump_journal(&format!("{tenant_name}:Payment:o1"));
    let target_event = target
        .iter()
        .find(|event| event.event_type == "AuthorizePayment")
        .expect("target event must be durable");
    let receipt = extract_receipt(&target_event.payload)
        .expect("receipt must decode")
        .expect("target event must contain receipt");
    assert_eq!(receipt.delivery_id, intents[0].delivery_id);
    assert_eq!(receipt.fencing_token, record.fencing_token);
}

#[tokio::test]
async fn non_awaited_delivery_materializes_and_wakes_descendant_reactions() {
    let tenant_name = "non-await-descendant";
    let (state, store) = build_durable_state(tenant_name, TWO_STEP_REACTIONS);
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

    let context = AgentContext::default();
    state
        .dispatch_tenant_action_ext(
            &tenant,
            "Order",
            "o1",
            "ConfirmOrder",
            serde_json::json!({}),
            DispatchExtOptions {
                agent_ctx: &context,
                await_integration: false,
                await_reactions: false,
            },
        )
        .await
        .expect("source commit");

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if status(&state, &tenant, "Payment", "o1").await == "Captured" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("descendant delivery should be woken without an idle rescan");

    let target = store.dump_journal(&format!("{tenant_name}:Payment:o1"));
    let descendant = target
        .iter()
        .find(|event| event.event_type == "AuthorizePayment")
        .and_then(|event| extract_intents(&event.payload).ok())
        .and_then(|mut intents| intents.pop())
        .expect("target commit must carry its descendant intent");
    let lifecycle = store.dump_journal(&delivery_journal_id(&descendant));
    assert_eq!(
        lifecycle
            .last()
            .and_then(|event| serde_json::from_value::<ReactionDeliveryRecord>(
                event.payload.clone()
            )
            .ok())
            .map(|record| record.status),
        Some(ReactionDeliveryStatus::Succeeded)
    );
}

#[tokio::test]
async fn awaited_delivery_waits_for_the_complete_descendant_tree() {
    let tenant_name = "await-descendant";
    let (state, store) = build_durable_state(tenant_name, TWO_STEP_REACTIONS);
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

    assert_eq!(status(&state, &tenant, "Payment", "o1").await, "Captured");
    let source_intent = store
        .dump_journal(&format!("{tenant_name}:Order:o1"))
        .iter()
        .find(|event| event.event_type == "ConfirmOrder")
        .and_then(|event| extract_intents(&event.payload).ok())
        .and_then(|mut intents| intents.pop())
        .expect("source intent");
    let descendant_intent = store
        .dump_journal(&format!("{tenant_name}:Payment:o1"))
        .iter()
        .find(|event| event.event_type == "AuthorizePayment")
        .and_then(|event| extract_intents(&event.payload).ok())
        .and_then(|mut intents| intents.pop())
        .expect("descendant intent");
    for intent in [source_intent, descendant_intent] {
        let latest = store
            .dump_journal(&delivery_journal_id(&intent))
            .pop()
            .expect("delivery lifecycle");
        let record: ReactionDeliveryRecord =
            serde_json::from_value(latest.payload).expect("delivery record");
        assert_eq!(record.status, ReactionDeliveryStatus::Succeeded);
    }
}

#[tokio::test]
async fn awaited_durable_reaction_reports_permanent_target_failure_after_source_commit() {
    let (_guard, _clock, _ids) = install_deterministic_context(417);
    let reactions = r#"
[[reaction]]
name = "invalid_capture"
[reaction.when]
entity_type = "Order"
action = "ConfirmOrder"
to_state = "Confirmed"
[reaction.then]
entity_type = "Payment"
action = "CapturePayment"
[reaction.resolve_target]
type = "same_id"
"#;
    let tenant_name = "shop-await-failure-417";
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

    let error = state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "o1",
            "ConfirmOrder",
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await
        .expect_err("awaited target rejection must be reported");
    assert!(error.contains("Rejected"), "unexpected error: {error}");
    assert_eq!(status(&state, &tenant, "Order", "o1").await, "Confirmed");

    let source = store.dump_journal(&format!("{tenant_name}:Order:o1"));
    let intent = extract_intents(
        &source
            .iter()
            .find(|event| event.event_type == "ConfirmOrder")
            .expect("committed source event")
            .payload,
    )
    .expect("source intent")
    .pop()
    .expect("one intent");
    let lifecycle = store.dump_journal(&delivery_journal_id(&intent));
    let record: ReactionDeliveryRecord =
        serde_json::from_value(lifecycle.last().expect("delivery outcome").payload.clone())
            .expect("delivery record");
    assert_eq!(record.status, ReactionDeliveryStatus::Rejected);
}

#[tokio::test]
async fn drop_ok_turns_permanent_target_failure_into_accepted_terminal_drop() {
    let (_guard, _clock, _ids) = install_deterministic_context(418);
    let reactions = r#"
[[reaction]]
name = "best_effort_capture"
drop_ok = true
[reaction.when]
entity_type = "Order"
action = "ConfirmOrder"
to_state = "Confirmed"
[reaction.then]
entity_type = "Payment"
action = "CapturePayment"
[reaction.resolve_target]
type = "same_id"
"#;
    let tenant_name = "shop-drop-ok-418";
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
            .expect("committed source event")
            .payload,
    )
    .expect("source intent")
    .pop()
    .expect("one intent");
    let lifecycle = store.dump_journal(&delivery_journal_id(&intent));
    let record: ReactionDeliveryRecord =
        serde_json::from_value(lifecycle.last().expect("delivery outcome").payload.clone())
            .expect("delivery record");
    assert_eq!(record.status, ReactionDeliveryStatus::DroppedAllowed);
}
