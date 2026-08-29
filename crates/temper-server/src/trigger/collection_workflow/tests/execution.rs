use super::*;
use crate::trigger::delivery::{
    AwaitedCallbackReceiptV1, AwaitedExecutionIdentityV1, AwaitedExecutionPhase, DeliveryKind,
    ReactionDeliveryRecord, ReactionDeliveryStatus, ReactionReceipt, append_delivery_record,
    attach_receipt, extract_intents,
};

pub(super) fn actions() -> CollectionExecutionActions<'static> {
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

pub(super) async fn commit_target_receipt(
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
        awaited_callback: None,
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

#[tokio::test(start_paused = true)]
async fn renewal_driver_keeps_the_exact_owner_live_and_replays_completed_evidence() {
    let (_guard, _clock, _ids) = temper_runtime::scheduler::install_deterministic_context(714);
    let tenant = "execution-renewal-driver";
    let sim = temper_store_sim::SimEventStore::no_faults(714);
    let store = BoxedEventStore::new(sim.clone());
    let (start_intent, mut workflow) =
        CollectionWorkflowRecordV1::start(start(tenant, "batch-1", &["a"])).unwrap();
    commit_activated_start(
        &store,
        source_append(tenant, "batch-1", 0, "StartChecks"),
        &start_intent,
        &mut workflow,
        &actions(),
        None,
    )
    .await
    .unwrap();
    let workflow_events = store
        .read_events(
            &collection_workflow_journal_id(tenant, &workflow.workflow_id),
            0,
        )
        .await
        .unwrap();
    let mut intent = extract_intents(&workflow_events[0].payload)
        .unwrap()
        .remove(0);
    commit_target_receipt(&store, &mut intent, 0).await;

    let now = sim_now();
    let deadline = now + chrono::Duration::seconds(90);
    intent.collection.as_mut().unwrap().execution_deadline = Some(deadline);
    let mut delivery = ReactionDeliveryRecord::pending(intent.clone());
    let fence = delivery.claim(now, chrono::Duration::seconds(30)).unwrap();
    delivery.begin_dispatch(fence).unwrap();
    let sequence = append_delivery_record(&store, 0, &delivery).await.unwrap();
    let owner = crate::trigger::dispatcher::AwaitedExecutionOwner::new(
        store.clone(),
        delivery,
        sequence,
        deadline,
    );
    let executing = owner
        .bind(
            "check",
            "check.wasm",
            "sha256:abc",
            "Succeeded",
            Some("Failed"),
            now,
        )
        .await
        .unwrap();
    assert_eq!(executing.phase, AwaitedExecutionPhase::Executing);
    let execution_id = owner
        .snapshot()
        .await
        .0
        .awaited_execution
        .as_ref()
        .unwrap()
        .identity
        .execution_id
        .clone();

    let wake_started = tokio::time::Instant::now();
    let result = crate::trigger::dispatcher::run_with_renewal(
        &owner,
        &intent.delivery_id,
        intent.kind,
        async {
            tokio::time::sleep(std::time::Duration::from_secs(35)).await;
            "module-result"
        },
        || {
            now + chrono::Duration::from_std(wake_started.elapsed())
                .expect("Tokio elapsed duration fits chrono")
        },
    )
    .await
    .unwrap();
    assert_eq!(result, "module-result");
    let (renewed, renewed_sequence) = owner.snapshot().await;
    assert!(renewed.lease_expires_at.unwrap() >= now + chrono::Duration::seconds(60));
    assert!(
        renewed_sequence >= sequence + 4,
        "bind plus three 10s renewals"
    );

    sim.fail_next_reads(
        &collection_workflow_journal_id(tenant, &workflow.workflow_id),
        1,
    );
    assert_eq!(
        owner
            .renew(now + chrono::Duration::seconds(40))
            .await
            .unwrap_err()
            .to_string(),
        "execution evidence storage failure"
    );
    owner
        .renew(now + chrono::Duration::seconds(40))
        .await
        .expect("owner state rolls back after a workflow read failure");

    owner
        .complete(
            &execution_id,
            true,
            Some("Succeeded"),
            Some(serde_json::json!({"ok": true})),
            None,
            now + chrono::Duration::seconds(35),
        )
        .await
        .unwrap();
    let (persisted, persisted_sequence) =
        crate::trigger::delivery::load_delivery_record(&store, intent.clone())
            .await
            .unwrap();
    drop(owner);

    let restarted = crate::trigger::dispatcher::AwaitedExecutionOwner::new(
        store,
        persisted,
        persisted_sequence,
        deadline,
    );
    let replay = restarted
        .bind(
            "check",
            "check.wasm",
            "sha256:abc",
            "Succeeded",
            Some("Failed"),
            now + chrono::Duration::seconds(36),
        )
        .await
        .unwrap();
    assert_eq!(replay.phase, AwaitedExecutionPhase::ExecutionSucceeded);
    assert_eq!(replay.callback_action.as_deref(), Some("Succeeded"));
    assert_eq!(
        replay.callback_params,
        Some(serde_json::json!({"ok": true}))
    );
}

#[tokio::test]
async fn cancellation_atomically_revokes_an_in_flight_awaited_owner() {
    let (_guard, _clock, _ids) = temper_runtime::scheduler::install_deterministic_context(715);
    let tenant = "execution-cancellation-race";
    let store = BoxedEventStore::new(temper_store_sim::SimEventStore::no_faults(715));
    let (start_intent, mut workflow) =
        CollectionWorkflowRecordV1::start(start(tenant, "batch-1", &["a"])).unwrap();
    commit_activated_start(
        &store,
        source_append(tenant, "batch-1", 0, "StartChecks"),
        &start_intent,
        &mut workflow,
        &actions(),
        None,
    )
    .await
    .unwrap();
    let workflow_events = store
        .read_events(
            &collection_workflow_journal_id(tenant, &workflow.workflow_id),
            0,
        )
        .await
        .unwrap();
    let mut intent = extract_intents(&workflow_events[0].payload)
        .unwrap()
        .remove(0);
    commit_target_receipt(&store, &mut intent, 0).await;

    let now = sim_now();
    let deadline = now + chrono::Duration::minutes(2);
    intent.collection.as_mut().unwrap().execution_deadline = Some(deadline);
    let mut delivery = ReactionDeliveryRecord::pending(intent.clone());
    let fence = delivery.claim(now, chrono::Duration::seconds(30)).unwrap();
    delivery.begin_dispatch(fence).unwrap();
    let sequence = append_delivery_record(&store, 0, &delivery).await.unwrap();
    let owner = crate::trigger::dispatcher::AwaitedExecutionOwner::new(
        store.clone(),
        delivery,
        sequence,
        deadline,
    );
    owner
        .bind(
            "check",
            "check.wasm",
            "sha256:abc",
            "Succeeded",
            Some("Failed"),
            now,
        )
        .await
        .unwrap();
    let execution_id = owner
        .snapshot()
        .await
        .0
        .awaited_execution
        .as_ref()
        .unwrap()
        .identity
        .execution_id
        .clone();

    let (mut current, workflow_sequence) =
        load_collection_record(&store, tenant, &workflow.workflow_id)
            .await
            .unwrap()
            .unwrap();
    let (control, _) = current
        .request_control(
            CollectionRequestedOutcome::Cancelled,
            None,
            "CancelChecks".to_string(),
            2,
            serde_json::json!({"principal": "controller"}),
            None,
        )
        .unwrap();
    commit_collection_control(
        &store,
        source_append(tenant, "batch-1", 1, "CancelChecks"),
        &control,
        workflow_sequence,
        &current,
    )
    .await
    .unwrap();

    assert_eq!(
        owner
            .renew(now + chrono::Duration::seconds(10))
            .await
            .unwrap_err()
            .to_string(),
        "execution fence lost"
    );
    assert!(
        owner
            .complete(
                &execution_id,
                true,
                Some("Succeeded"),
                Some(serde_json::json!({"late": true})),
                None,
                now + chrono::Duration::seconds(10),
            )
            .await
            .is_err()
    );
    let (persisted, _) = crate::trigger::delivery::load_delivery_record(&store, intent)
        .await
        .unwrap();
    assert_eq!(
        persisted.awaited_execution.unwrap().phase,
        AwaitedExecutionPhase::Executing,
        "control and completion cannot both win the workflow fence"
    );
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
        None,
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
    for intent in &intents {
        let rule: crate::trigger::types::ReactionRule =
            serde_json::from_value(intent.rule.clone()).expect("bound collection rule");
        assert_eq!(
            rule.principal.as_deref(),
            Some("wasm-runtime"),
            "kernel-owned collection deliveries must not inherit caller authority"
        );
    }
}

#[tokio::test]
async fn callback_target_and_exact_execution_acceptance_commit_atomically() {
    let tenant = "execution-callback";
    let store = BoxedEventStore::new(temper_store_sim::SimEventStore::no_faults(713));
    let (start_intent, mut workflow) =
        CollectionWorkflowRecordV1::start(start(tenant, "batch-1", &["a"])).unwrap();
    commit_activated_start(
        &store,
        source_append(tenant, "batch-1", 0, "StartChecks"),
        &start_intent,
        &mut workflow,
        &actions(),
        None,
    )
    .await
    .unwrap();
    let events = store
        .read_events(
            &collection_workflow_journal_id(tenant, &workflow.workflow_id),
            0,
        )
        .await
        .unwrap();
    let mut intent = extract_intents(&events[0].payload).unwrap().remove(0);
    commit_target_receipt(&store, &mut intent, 0).await;

    let now = sim_now();
    let mut delivery = ReactionDeliveryRecord::pending(intent.clone());
    let fence = delivery.claim(now, chrono::Duration::seconds(30)).unwrap();
    delivery.begin_dispatch(fence).unwrap();
    delivery
        .bind_awaited_execution(
            fence,
            AwaitedExecutionIdentityV1 {
                execution_id: "execution-1".to_string(),
                integration_name: "check".to_string(),
                module_name: "check.wasm".to_string(),
                module_digest: "sha256:abc".to_string(),
                success_callback: "Succeeded".to_string(),
                failure_callback: Some("Failed".to_string()),
                schema_pin: None,
                deadline: now + chrono::Duration::minutes(1),
            },
            now,
        )
        .unwrap();
    delivery
        .record_awaited_completion(
            fence,
            "execution-1",
            true,
            Some("Succeeded"),
            Some(serde_json::json!({"ok": true})),
            None,
            now,
        )
        .unwrap();
    let delivery_sequence = append_delivery_record(&store, 0, &delivery).await.unwrap();
    let receipt = ReactionReceipt {
        delivery_id: intent.delivery_id.clone(),
        fencing_token: fence,
        received_at: now,
        state_timeout_state: None,
        schema_pin: None,
        collection: intent.collection.clone(),
        awaited_callback: Some(AwaitedCallbackReceiptV1 {
            execution_id: "execution-1".to_string(),
            callback_action: "Succeeded".to_string(),
        }),
    };
    let mut target = source_append(
        tenant,
        intent.target_entity_id.as_deref().unwrap(),
        1,
        "Succeeded",
    );
    target.persistence_id = format!(
        "{tenant}:{}:{}",
        actions().member_entity,
        intent.target_entity_id.as_deref().unwrap()
    );
    let appends = target_fence_appends(&store, tenant, &receipt, 2)
        .await
        .unwrap();
    let mut batch = vec![target];
    batch.extend(appends);
    store.append_batch(&batch).await.unwrap();

    let (accepted, sequence) =
        crate::trigger::delivery::load_delivery_record(&store, intent.clone())
            .await
            .unwrap();
    assert_eq!(sequence, delivery_sequence + 1);
    assert_eq!(
        accepted.awaited_execution.unwrap().phase,
        AwaitedExecutionPhase::CallbackAccepted
    );

    let mut stale = receipt;
    stale.fencing_token += 1;
    assert!(
        target_fence_appends(&store, tenant, &stale, 3)
            .await
            .is_err()
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
    let cancellation_rule: crate::trigger::types::ReactionRule =
        serde_json::from_value(cancellation[0].rule.clone()).unwrap();
    assert_eq!(cancellation_rule.principal.as_deref(), Some("wasm-runtime"));
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
    let join_rule: crate::trigger::types::ReactionRule =
        serde_json::from_value(joins[0].rule.clone()).unwrap();
    assert_eq!(join_rule.principal.as_deref(), Some("wasm-runtime"));
    assert_eq!(
        joins[0].authority,
        serde_json::json!({"principal": "test-agent"})
    );
}

#[tokio::test]
async fn generated_control_races_converge_through_private_production_commits() {
    for seed in 730..746 {
        let tenant = format!("execution-control-{seed}");
        let sim = temper_store_sim::SimEventStore::no_faults(seed);
        let store = BoxedEventStore::new(sim.clone());
        let (start_intent, mut record) =
            CollectionWorkflowRecordV1::start(start(&tenant, "batch-1", &["a"])).unwrap();
        commit_activated_start(
            &store,
            source_append(&tenant, "batch-1", 0, "StartChecks"),
            &start_intent,
            &mut record,
            &actions(),
            None,
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

        let owner_now = sim_now();
        let owner_deadline = owner_now + chrono::Duration::minutes(2);
        member_intent
            .collection
            .as_mut()
            .unwrap()
            .execution_deadline = Some(owner_deadline);
        let mut owner_delivery = ReactionDeliveryRecord::pending(member_intent.clone());
        let owner_fence = owner_delivery
            .claim(owner_now, chrono::Duration::seconds(30))
            .unwrap();
        owner_delivery.begin_dispatch(owner_fence).unwrap();
        let owner_sequence = append_delivery_record(&store, 0, &owner_delivery)
            .await
            .unwrap();
        let mut owner = crate::trigger::dispatcher::AwaitedExecutionOwner::new(
            store.clone(),
            owner_delivery,
            owner_sequence,
            owner_deadline,
        );
        owner
            .bind(
                "check",
                "check.wasm",
                "sha256:generated",
                "Succeeded",
                Some("Failed"),
                owner_now,
            )
            .await
            .unwrap();
        let execution_id = owner
            .snapshot()
            .await
            .0
            .awaited_execution
            .as_ref()
            .unwrap()
            .identity
            .execution_id
            .clone();

        let workflow_journal = collection_workflow_journal_id(&tenant, &record.workflow_id);
        match seed % 3 {
            0 => {
                sim.fail_next_reads(&workflow_journal, 1);
                assert_eq!(
                    owner
                        .renew(owner_now + chrono::Duration::seconds(10))
                        .await
                        .unwrap_err()
                        .to_string(),
                    "execution evidence storage failure"
                );
                owner
                    .renew(owner_now + chrono::Duration::seconds(10))
                    .await
                    .unwrap();
            }
            1 => {
                sim.inject_concurrency_violations(&workflow_journal, 1);
                assert_eq!(
                    owner
                        .renew(owner_now + chrono::Duration::seconds(10))
                        .await
                        .unwrap_err()
                        .to_string(),
                    "execution fence lost"
                );
                owner
                    .renew(owner_now + chrono::Duration::seconds(10))
                    .await
                    .unwrap();
            }
            _ => {
                let (recovered, recovered_sequence) =
                    crate::trigger::delivery::load_delivery_record(&store, member_intent.clone())
                        .await
                        .unwrap();
                drop(owner);
                owner = crate::trigger::dispatcher::AwaitedExecutionOwner::new(
                    store.clone(),
                    recovered,
                    recovered_sequence,
                    owner_deadline,
                );
                assert_eq!(
                    owner
                        .bind(
                            "check",
                            "check.wasm",
                            "sha256:generated",
                            "Succeeded",
                            Some("Failed"),
                            owner_now + chrono::Duration::seconds(10),
                        )
                        .await
                        .unwrap()
                        .phase,
                    AwaitedExecutionPhase::Executing
                );
            }
        }

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
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            owner
                .renew(owner_now + chrono::Duration::seconds(20))
                .await
                .unwrap_err()
                .to_string(),
            "execution fence lost"
        );
        assert!(
            owner
                .complete(
                    &execution_id,
                    true,
                    Some("Succeeded"),
                    Some(serde_json::json!({"late": true})),
                    None,
                    owner_now + chrono::Duration::seconds(20),
                )
                .await
                .is_err()
        );

        let stale_receipt = ReactionReceipt {
            delivery_id: member_intent.delivery_id.clone(),
            fencing_token: 2,
            received_at: sim_now(),
            state_timeout_state: None,
            schema_pin: None,
            collection: member_intent.collection.clone(),
            awaited_callback: None,
        };
        assert!(
            target_fence_append(&store, &tenant, &stale_receipt)
                .await
                .is_err()
        );

        let (mut original, original_sequence) =
            crate::trigger::delivery::load_delivery_record(&store, member_intent.clone())
                .await
                .unwrap();
        original.status = ReactionDeliveryStatus::Succeeded;
        original.lease_expires_at = None;
        assert!(
            commit_terminal_delivery(&store, original_sequence, &original)
                .await
                .expect("controlled receipted original closes before its cancellation")
        );
        let (closed_original, _) =
            crate::trigger::delivery::load_delivery_record(&store, member_intent.clone())
                .await
                .unwrap();
        assert_eq!(closed_original.status, ReactionDeliveryStatus::Succeeded);

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
