use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_store_sim::SimFaultConfig;

use super::field_update_support::*;
use super::*;

#[tokio::test]
async fn dst_field_update_ambiguous_commit_replays_as_success_without_duplicate() {
    let seed = 18_907;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let scripted_store = ConflictBeforeAppendStore::new(seed);
    let store_inner = scripted_store.inner.clone();
    let store = BoxedEventStore::new(scripted_store.clone());
    let table = order_table();
    let entity_id = "ord-field-ambiguous-commit";
    let persistence_id = format!("default:Order:{entity_id}");
    let system = ActorSystem::new("dst-field-update-ambiguous-commit");
    let actor = EntityActor::with_persistence(
        "Order",
        entity_id,
        table,
        serde_json::json!({"Title": "durable-before"}),
        store,
        BackendLabel::Sim,
    )
    .with_tenant("default");
    let actor_ref = system.spawn(actor, entity_id);

    let before = get_state(&actor_ref).await;
    assert_eq!(before.state.sequence_nr, 1);
    scripted_store.commit_then_report_conflict(&persistence_id);
    let idempotency_key = "field-update:ambiguous-commit".to_string();

    let response = actor_ref
        .ask::<EntityResponse>(
            EntityMsg::UpdateFields {
                fields: serde_json::json!({"Title": "durable-after"}),
                replace: false,
                idempotency_key: Some(idempotency_key.clone()),
                expected_spec_generation: None,
            },
            Duration::from_secs(5),
        )
        .await
        .expect("authoritative replay should recognize the committed update");
    assert!(response.success);
    assert_eq!(response.state.sequence_nr, 2);
    assert_eq!(response.state.fields["Title"], "durable-after");
    assert_eq!(store_inner.dump_journal(&persistence_id).len(), 2);

    let retried = actor_ref
        .ask::<EntityResponse>(
            EntityMsg::UpdateFields {
                fields: serde_json::json!({"Title": "durable-after"}),
                replace: false,
                idempotency_key: Some(idempotency_key),
                expected_spec_generation: None,
            },
            Duration::from_secs(5),
        )
        .await
        .expect("an ask retry with the same key should return committed state");
    assert!(retried.success);
    assert_eq!(retried.state.sequence_nr, 2);
    assert_eq!(store_inner.dump_journal(&persistence_id).len(), 2);
}

#[tokio::test]
async fn dst_field_update_storage_error_after_commit_recovers_without_duplicate() {
    let seed = 18_908;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let scripted_store = ConflictBeforeAppendStore::new(seed);
    let store_inner = scripted_store.inner.clone();
    let store = BoxedEventStore::new(scripted_store.clone());
    let table = order_table();
    let entity_id = "ord-field-ambiguous-storage";
    let persistence_id = format!("default:Order:{entity_id}");
    let system = ActorSystem::new("dst-field-update-ambiguous-storage");
    let actor = EntityActor::with_persistence(
        "Order",
        entity_id,
        table,
        serde_json::json!({"Title": "durable-before"}),
        store,
        BackendLabel::Sim,
    )
    .with_tenant("default");
    let actor_ref = system.spawn(actor, entity_id);

    let before = get_state(&actor_ref).await;
    assert_eq!(before.state.sequence_nr, 1);
    scripted_store.commit_then_report_storage_failure(&persistence_id);
    let idempotency_key = "field-update:ambiguous-storage".to_string();

    let response = actor_ref
        .ask::<EntityResponse>(
            EntityMsg::UpdateFields {
                fields: serde_json::json!({"Title": "durable-after"}),
                replace: false,
                idempotency_key: Some(idempotency_key.clone()),
                expected_spec_generation: None,
            },
            Duration::from_secs(5),
        )
        .await
        .expect("authoritative replay should recognize the durably committed update");
    assert!(response.success);
    assert_eq!(response.state.sequence_nr, 2);
    assert_eq!(response.state.fields["Title"], "durable-after");
    assert_eq!(store_inner.dump_journal(&persistence_id).len(), 2);

    let retried = actor_ref
        .ask::<EntityResponse>(
            EntityMsg::UpdateFields {
                fields: serde_json::json!({"Title": "durable-after"}),
                replace: false,
                idempotency_key: Some(idempotency_key),
                expected_spec_generation: None,
            },
            Duration::from_secs(5),
        )
        .await
        .expect("retry should return the recovered committed state");
    assert!(retried.success);
    assert_eq!(retried.state.sequence_nr, 2);
    assert_eq!(store_inner.dump_journal(&persistence_id).len(), 2);
}

#[tokio::test]
async fn dst_field_update_retry_exhaustion_is_reported_distinctly() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(18_902);
    let store_inner = SimEventStore::no_faults(18_902);
    let store = BoxedEventStore::new(store_inner.clone());
    let table = order_table();
    let entity_id = "ord-field-concurrency-exhausted";
    let persistence_id = format!("default:Order:{entity_id}");
    let system = ActorSystem::new("dst-field-update-concurrency-exhausted");
    let actor = EntityActor::with_persistence(
        "Order",
        entity_id,
        table,
        serde_json::json!({"Title": "durable-before"}),
        store,
        BackendLabel::Sim,
    )
    .with_tenant("default");
    let actor_ref = system.spawn(actor, entity_id);

    let before = get_state(&actor_ref).await;
    assert_eq!(before.state.sequence_nr, 1);
    store_inner.inject_concurrency_violations(&persistence_id, 4);

    let response = update_fields(
        &actor_ref,
        serde_json::json!({"Title": "must-not-publish"}),
        false,
    )
    .await;

    assert!(!response.success, "exhausted retries must fail the caller");
    assert_eq!(
        response.error.as_deref(),
        Some("field update retry budget exhausted"),
        "retry exhaustion must remain distinguishable from a non-concurrency persistence failure"
    );
    assert_eq!(response.state.fields["Title"], "durable-before");
    assert_eq!(response.state.sequence_nr, before.state.sequence_nr);
    assert_eq!(
        store_inner.pending_concurrency_violations(&persistence_id),
        1,
        "one initial attempt plus two retries must consume exactly three violations"
    );
    assert_eq!(
        store_inner.dump_journal(&persistence_id).len(),
        1,
        "failed field updates must not append or publish speculative state"
    );
}

#[tokio::test]
async fn dst_field_update_retry_exhaustion_keeps_recovered_history() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(18_903);
    let store_inner = SimEventStore::no_faults(18_903);
    let store = BoxedEventStore::new(store_inner.clone());
    let table = order_table();
    let entity_id = "ord-field-recovered-on-exhaustion";
    let persistence_id = format!("default:Order:{entity_id}");
    let system = ActorSystem::new("dst-field-recovered-on-exhaustion");
    let actor = EntityActor::with_persistence(
        "Order",
        entity_id,
        table,
        serde_json::json!({"Title": "durable-before"}),
        store,
        BackendLabel::Sim,
    )
    .with_tenant("default");
    let actor_ref = system.spawn(actor, entity_id);

    let before = get_state(&actor_ref).await;
    assert_eq!(before.state.sequence_nr, 1);
    store_inner
        .append(
            &persistence_id,
            1,
            &[PersistenceEnvelope {
                sequence_nr: 2,
                event_type: "AddItem".to_string(),
                payload: serde_json::json!({
                    "action": "AddItem",
                    "from_status": "Draft",
                    "to_status": "Draft",
                    "timestamp": sim_now(),
                    "params": {"ProductId": "concurrent-item"}
                }),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp: sim_now(),
                    actor_id: persistence_id.clone(),
                },
            }],
        )
        .await
        .expect("concurrent action should advance authoritative history");
    store_inner.inject_concurrency_violations(&persistence_id, 3);

    let response = update_fields(
        &actor_ref,
        serde_json::json!({"Title": "must-not-publish"}),
        false,
    )
    .await;

    assert!(!response.success, "exhausted retries must fail the PATCH");
    assert_eq!(
        response.error.as_deref(),
        Some("field update retry budget exhausted")
    );
    assert_eq!(
        response.state.sequence_nr, 2,
        "the actor must retain the authoritative sequence recovered during retry"
    );
    assert_eq!(response.state.item_count, 1);
    assert_eq!(response.state.fields["ProductId"], "concurrent-item");
    assert_eq!(response.state.fields["Title"], "durable-before");

    let live = get_state(&actor_ref).await;
    assert_eq!(live.state.sequence_nr, 2);
    assert_eq!(live.state.item_count, 1);
    assert_eq!(live.state.fields["ProductId"], "concurrent-item");
    assert_eq!(live.state.fields["Title"], "durable-before");
    assert_eq!(
        store_inner.dump_journal(&persistence_id).len(),
        2,
        "the failed PATCH must not append speculative history"
    );
}

#[tokio::test]
async fn dst_field_update_retry_exhaustion_recovers_the_final_real_writer() {
    let seed = 18_904;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let scripted_store = ConflictBeforeAppendStore::new(seed);
    let store_inner = scripted_store.inner.clone();
    let store = BoxedEventStore::new(scripted_store.clone());
    let table = order_table();
    let entity_id = "ord-field-final-real-writer";
    let persistence_id = format!("default:Order:{entity_id}");
    let system = ActorSystem::new("dst-field-final-real-writer");
    let actor = EntityActor::with_persistence(
        "Order",
        entity_id,
        table,
        serde_json::json!({"Title": "durable-before"}),
        store,
        BackendLabel::Sim,
    )
    .with_tenant("default");
    let actor_ref = system.spawn(actor, entity_id);

    let before = get_state(&actor_ref).await;
    assert_eq!(before.state.sequence_nr, 1);
    scripted_store.queue_conflicts(
        &persistence_id,
        vec![
            concurrent_add_item(&persistence_id, "concurrent-item-1"),
            concurrent_add_item(&persistence_id, "concurrent-item-2"),
            concurrent_add_item(&persistence_id, "concurrent-item-3"),
        ],
    );

    let response = update_fields(
        &actor_ref,
        serde_json::json!({"Title": "must-not-publish"}),
        false,
    )
    .await;

    assert!(!response.success, "exhausted retries must fail the PATCH");
    assert_eq!(
        response.error.as_deref(),
        Some("field update retry budget exhausted")
    );
    assert_eq!(
        response.state.sequence_nr, 4,
        "the exhausted response must include the writer that won the final attempt"
    );
    assert_eq!(response.state.item_count, 3);
    assert_eq!(response.state.fields["ProductId"], "concurrent-item-3");
    assert_eq!(response.state.fields["Title"], "durable-before");

    let live = get_state(&actor_ref).await;
    assert_eq!(live.state.sequence_nr, 4);
    assert_eq!(live.state.item_count, 3);
    assert_eq!(live.state.fields["ProductId"], "concurrent-item-3");
    assert_eq!(live.state.fields["Title"], "durable-before");
    assert_eq!(
        store_inner.dump_journal(&persistence_id).len(),
        4,
        "only the three real concurrent writers may follow Created"
    );
}

#[tokio::test]
async fn dst_field_update_recovery_read_failure_restarts_before_serving_state() {
    let seed = 18_905;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let scripted_store = ConflictBeforeAppendStore::new(seed);
    let store_inner = scripted_store.inner.clone();
    let store = BoxedEventStore::new(scripted_store.clone());
    let table = order_table();
    let entity_id = "ord-field-recovery-read-failure";
    let persistence_id = format!("default:Order:{entity_id}");
    let system = ActorSystem::new("dst-field-recovery-read-failure");
    let actor = EntityActor::with_persistence(
        "Order",
        entity_id,
        table,
        serde_json::json!({"Title": "durable-before"}),
        store,
        BackendLabel::Sim,
    )
    .with_tenant("default");
    let actor_ref = system.spawn(actor, entity_id);

    let before = get_state(&actor_ref).await;
    assert_eq!(before.state.sequence_nr, 1);
    scripted_store.queue_conflicts(
        &persistence_id,
        vec![concurrent_add_item(
            &persistence_id,
            "concurrent-item-after-read-failure",
        )],
    );
    store_inner.fail_next_reads(&persistence_id, 1);

    let update = actor_ref
        .ask::<EntityResponse>(
            EntityMsg::UpdateFields {
                fields: serde_json::json!({"Title": "must-not-publish"}),
                replace: false,
                idempotency_key: Some("field-update:recovery-read-failure".to_string()),
                expected_spec_generation: None,
            },
            Duration::from_secs(5),
        )
        .await;
    assert!(
        update.is_err(),
        "failed authoritative recovery must fail the actor message so stale state cannot remain live"
    );

    let recovered = get_state(&actor_ref).await;
    assert_eq!(
        recovered.state.sequence_nr, 2,
        "the restarted actor must replay the real writer before serving state"
    );
    assert_eq!(recovered.state.item_count, 1);
    assert_eq!(
        recovered.state.fields["ProductId"],
        "concurrent-item-after-read-failure"
    );
    assert_eq!(recovered.state.fields["Title"], "durable-before");
    assert_eq!(store_inner.dump_journal(&persistence_id).len(), 2);
}

#[tokio::test]
async fn dst_field_update_restart_rejects_a_successful_looking_journal_prefix() {
    let seed = 18_906;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let scripted_store = ConflictBeforeAppendStore::new(seed);
    let store_inner = scripted_store.inner.clone();
    let store = BoxedEventStore::new(scripted_store.clone());
    let table = order_table();
    let entity_id = "ord-field-restart-truncated-tail";
    let persistence_id = format!("default:Order:{entity_id}");
    let system = ActorSystem::new("dst-field-restart-truncated-tail");
    let actor = EntityActor::with_persistence(
        "Order",
        entity_id,
        table.clone(),
        serde_json::json!({"Title": "durable-before"}),
        store.clone(),
        BackendLabel::Sim,
    )
    .with_tenant("default");
    let actor_ref = system.spawn(actor, entity_id);

    let before = get_state(&actor_ref).await;
    assert_eq!(before.state.sequence_nr, 1);
    scripted_store.queue_conflicts(
        &persistence_id,
        vec![concurrent_add_item(
            &persistence_id,
            "concurrent-item-before-prefix",
        )],
    );
    store_inner.restore_faults(SimFaultConfig {
        read_truncation_prob: 1.0,
        ..SimFaultConfig::none()
    });
    store_inner.fail_next_reads(&persistence_id, 1);

    let update = actor_ref
        .ask::<EntityResponse>(
            EntityMsg::UpdateFields {
                fields: serde_json::json!({"Title": "must-not-publish"}),
                replace: false,
                idempotency_key: Some("field-update:truncated-tail".to_string()),
                expected_spec_generation: None,
            },
            Duration::from_secs(5),
        )
        .await;
    assert!(
        update.is_err(),
        "the explicit recovery failure must enter actor supervision"
    );

    let stale = actor_ref
        .ask::<EntityResponse>(EntityMsg::GetState, Duration::from_secs(5))
        .await;
    assert!(
        stale.is_err(),
        "supervision must not publish a successful-looking prefix as current state"
    );

    store_inner.disable_faults();
    let replacement = EntityActor::with_persistence(
        "Order",
        entity_id,
        table,
        serde_json::json!({"Title": "durable-before"}),
        store,
        BackendLabel::Sim,
    )
    .with_tenant("default");
    let replacement_ref = system.spawn(replacement, "replacement-after-truncated-tail");
    let recovered = get_state(&replacement_ref).await;
    assert_eq!(recovered.state.sequence_nr, 2);
    assert_eq!(recovered.state.item_count, 1);
    assert_eq!(
        recovered.state.fields["ProductId"],
        "concurrent-item-before-prefix"
    );
    assert_eq!(recovered.state.fields["Title"], "durable-before");
    assert_eq!(store_inner.dump_journal(&persistence_id).len(), 2);
}

#[tokio::test]
async fn dst_reserved_field_update_event_type_cannot_be_dispatched_as_action() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(18_901);
    let store_inner = SimEventStore::no_faults(18_901);
    let store = BoxedEventStore::new(store_inner.clone());
    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "ReservedAction"
states = ["Draft", "Updated"]
initial = "Draft"

[[action]]
name = "$temper.entity.fields-updated.v1"
kind = "input"
from = ["Draft"]
to = "Updated"
"#,
    )));
    let system = ActorSystem::new("dst-reserved-field-update-event");
    let actor = EntityActor::with_persistence(
        "ReservedAction",
        "reserved-action-1",
        table,
        serde_json::json!({}),
        store,
        BackendLabel::Sim,
    )
    .with_tenant("default");
    let actor_ref = system.spawn(actor, "reserved-action-1");

    let response = dispatch_action(
        &actor_ref,
        "$temper.entity.fields-updated.v1",
        serde_json::json!({}),
    )
    .await;
    assert!(
        !response.success,
        "reserved journal type must not run as an action"
    );
    assert_eq!(response.state.status, "Draft");
    let journal = store_inner.dump_journal("default:ReservedAction:reserved-action-1");
    assert_eq!(journal.len(), 1, "only the bootstrap event may be durable");
    assert_eq!(journal[0].event_type, "Created");
}
