use super::*;
use crate::trigger::delivery::{
    DeliveryKind, ReactionDeliveryRecord, ReactionDeliveryStatus, ReactionReceipt, attach_receipt,
    extract_intents,
};

fn actions() -> CollectionExecutionActions<'static> {
    CollectionExecutionActions {
        member_entity: "CheckRun",
        member_action: "Start",
        member_cancel_action: "Cancel",
        timeout_action: "TimeoutChecks",
        on_success: "ChecksSucceeded",
        on_partial_failure: "ChecksPartiallyFailed",
        on_failure: "ChecksFailed",
        on_cancelled: "ChecksCancelled",
        on_timed_out: "ChecksTimedOut",
    }
}

async fn commit_target_receipt(
    store: &BoxedEventStore,
    intent: &mut crate::trigger::delivery::PersistedReactionIntent,
    target_sequence: u64,
) {
    intent.collection.as_mut().unwrap().attempts = 1;
    let receipt = ReactionReceipt {
        delivery_id: intent.delivery_id.clone(),
        fencing_token: 1,
        received_at: sim_now(),
        state_timeout_state: None,
        schema_pin: intent.schema_pin.clone(),
        collection: intent.collection.clone(),
    };
    let mut target = source_append(
        &intent.tenant,
        intent.target_entity_id.as_deref().unwrap(),
        target_sequence,
        "TargetAction",
    );
    target.persistence_id = format!(
        "{}:{}:{}",
        intent.tenant,
        actions().member_entity,
        intent.target_entity_id.as_deref().unwrap()
    );
    attach_receipt(&mut target.events[0].payload, &receipt).unwrap();
    let fence = target_fence_append(store, &intent.tenant, &receipt)
        .await
        .unwrap();
    store.append_batch(&[target, fence]).await.unwrap();
}

#[tokio::test]
async fn activated_start_co_commits_bounded_member_intents_for_recovery() {
    let store = BoxedEventStore::new(temper_store_sim::SimEventStore::no_faults(712));
    let (intent, mut record) =
        CollectionWorkflowRecordV1::start(start("execution", "batch-1", &["a", "b", "c"]))
            .expect("valid start");
    commit_activated_start(
        &store,
        source_append("execution", "batch-1", 0, "StartChecks"),
        &intent,
        &mut record,
        &actions(),
    )
    .await
    .expect("activated start");
    assert_eq!(record.counts.in_flight, 2);
    assert_eq!(record.counts.pending, 1);
    let events = store
        .read_events(
            &collection_workflow_journal_id("execution", &record.workflow_id),
            0,
        )
        .await
        .unwrap();
    let intents = extract_intents(&events[0].payload).unwrap();
    assert_eq!(intents.len(), 2);
    assert!(
        intents
            .iter()
            .all(|intent| intent.kind == DeliveryKind::CollectionMember)
    );
    assert_eq!(
        intents[0].target_entity_id.as_deref(),
        Some(record.members[0].member_id.as_str())
    );
}

#[test]
fn recovery_reconstructs_cancel_and_exact_join_from_bound_actions() {
    let (_, mut record) =
        CollectionWorkflowRecordV1::start(start("execution-recovery", "batch-1", &["a"])).unwrap();
    activate_start(&mut record, 0, &actions()).unwrap();
    let member_id = record.members[0].member_id.clone();
    let delivery_id = record.members[0].delivery_id.clone().unwrap();
    let receipt = CollectionMemberReceipt {
        delivery_id: delivery_id.clone(),
        fencing_token: 1,
    };
    record
        .record_member_receipt(&member_id, &delivery_id, 0, 1, receipt)
        .unwrap();
    record
        .bind_timeout(CollectionTimeoutBinding {
            delivery_id: "timeout-execution-recovery".to_string(),
            timeout_action: "TimeoutChecks".to_string(),
            state: "Running".to_string(),
            deadline: sim_now() + chrono::Duration::seconds(60),
            declaration_id: "timeout-declaration".to_string(),
            clock_sequence: record.source_sequence,
            schema_digest: record.schema_digest.clone(),
        })
        .unwrap();
    record
        .request_control(
            CollectionRequestedOutcome::TimedOut,
            Some("timeout-execution-recovery"),
            "TimeoutChecks".to_string(),
            2,
            serde_json::json!({"principal": "timeout-scheduler"}),
            None,
        )
        .unwrap();
    let cancellation = recover_progress(&mut record, 2).unwrap();
    assert_eq!(cancellation.len(), 1);
    assert_eq!(cancellation[0].kind, DeliveryKind::CollectionCancellation);
    record
        .record_member_controlled_terminal(
            &member_id,
            &cancellation[0].delivery_id,
            1,
            ReactionDeliveryStatus::Succeeded,
            true,
        )
        .unwrap();
    let joins = recover_progress(&mut record, 3).unwrap();
    assert_eq!(joins.len(), 1);
    assert_eq!(joins[0].kind, DeliveryKind::CollectionJoin);
    assert_eq!(
        joins[0].authority,
        serde_json::json!({"principal": "test-agent"})
    );
}

#[tokio::test]
async fn generated_control_races_converge_through_private_production_commits() {
    for seed in 730..746 {
        let tenant = format!("execution-control-{seed}");
        let store = BoxedEventStore::new(temper_store_sim::SimEventStore::no_faults(seed));
        let (start_intent, mut record) =
            CollectionWorkflowRecordV1::start(start(&tenant, "batch-1", &["a"])).unwrap();
        commit_activated_start(
            &store,
            source_append(&tenant, "batch-1", 0, "StartChecks"),
            &start_intent,
            &mut record,
            &actions(),
        )
        .await
        .unwrap();
        let workflow_events = store
            .read_events(
                &collection_workflow_journal_id(&tenant, &record.workflow_id),
                0,
            )
            .await
            .unwrap();
        let mut member_intent = extract_intents(&workflow_events[0].payload)
            .unwrap()
            .remove(0);
        commit_target_receipt(&store, &mut member_intent, 0).await;

        let (mut controlled, workflow_sequence) =
            load_collection_record(&store, &tenant, &record.workflow_id)
                .await
                .unwrap()
                .unwrap();
        let requested = if seed % 2 == 0 {
            CollectionRequestedOutcome::Cancelled
        } else {
            CollectionRequestedOutcome::TimedOut
        };
        let source_action = if requested == CollectionRequestedOutcome::Cancelled {
            "CancelChecks"
        } else {
            "TimeoutChecks"
        };
        let timeout_delivery_id = (requested == CollectionRequestedOutcome::TimedOut).then(|| {
            controlled
                .timeout_binding
                .as_ref()
                .unwrap()
                .delivery_id
                .clone()
        });
        let (control, _) = controlled
            .request_control(
                requested,
                timeout_delivery_id.as_deref(),
                source_action.to_string(),
                2,
                serde_json::json!({"principal": "controller"}),
                None,
            )
            .unwrap();
        commit_controlled(
            &store,
            source_append(&tenant, "batch-1", 1, source_action),
            &control,
            workflow_sequence,
            &mut controlled,
        )
        .await
        .unwrap();

        let stale_receipt = ReactionReceipt {
            delivery_id: member_intent.delivery_id.clone(),
            fencing_token: 2,
            received_at: sim_now(),
            state_timeout_state: None,
            schema_pin: None,
            collection: member_intent.collection.clone(),
        };
        assert!(
            target_fence_append(&store, &tenant, &stale_receipt)
                .await
                .is_err()
        );

        let controlled_events = store
            .read_events(
                &collection_workflow_journal_id(&tenant, &record.workflow_id),
                workflow_sequence,
            )
            .await
            .unwrap();
        let mut cancellation = extract_intents(&controlled_events[0].payload)
            .unwrap()
            .remove(0);
        assert_eq!(cancellation.kind, DeliveryKind::CollectionCancellation);
        commit_target_receipt(&store, &mut cancellation, 1).await;
        let mut terminal = ReactionDeliveryRecord::pending(cancellation.clone());
        terminal.attempts = 1;
        terminal.fencing_token = 1;
        terminal.status = ReactionDeliveryStatus::Succeeded;
        assert!(
            commit_terminal_delivery(&store, 0, &terminal)
                .await
                .unwrap()
        );

        let mut original = ReactionDeliveryRecord::pending(member_intent.clone());
        original.attempts = 1;
        original.fencing_token = 1;
        original.status = ReactionDeliveryStatus::Succeeded;
        assert!(
            commit_terminal_delivery(&store, 0, &original)
                .await
                .expect("controlled receipted original closes as a workflow no-op")
        );
        let (closed_original, _) =
            crate::trigger::delivery::load_delivery_record(&store, member_intent.clone())
                .await
                .unwrap();
        assert_eq!(closed_original.status, ReactionDeliveryStatus::Succeeded);

        let (completed, _) = load_collection_record(&store, &tenant, &record.workflow_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(completed.requested_outcome, Some(requested));
        assert_eq!(completed.counts.terminal(), 1);
        assert_eq!(completed.join_status, CollectionJoinStatus::InFlight);
        let latest = store
            .read_latest_events(
                &collection_workflow_journal_id(&tenant, &record.workflow_id),
                1,
            )
            .await
            .unwrap();
        let joins = extract_intents(&latest[0].payload).unwrap();
        assert_eq!(joins.len(), 1);
        assert_eq!(joins[0].kind, DeliveryKind::CollectionJoin);
        assert_eq!(joins[0].authority, start_intent.start.authority);
    }
}
