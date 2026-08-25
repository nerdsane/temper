use super::*;

#[test]
fn identity_derivation_has_golden_vectors_and_child_equals_member() {
    let workflow_id = collection_workflow_id(
        "tenant-a",
        "Batch",
        "batch-7",
        "run_checks",
        "StartChecks",
        42,
        "sha256:feedface",
    );
    let member_id = collection_member_id(&workflow_id, 3, "check-雪");
    assert_eq!(
        workflow_id,
        "collection-workflow-v1-12f373322f0531282b4b933dd901c1075a9997e1b45066f44e7cb022f579576a"
    );
    assert_eq!(
        member_id,
        "collection-member-v1-3a8882ea22f287ee78f1b90ba93b8520a87361aba8b3c325227694db8d38ab68"
    );
    assert_eq!(collection_child_id(&workflow_id, 3, "check-雪"), member_id);
    assert_eq!(
        collection_control_id(&workflow_id, "CancelChecks", 43, "Cancelled"),
        "collection-control-v1-e7bcb4b43ae5b807ad94ff8d693acb9eb17a428577b775f32ccadd6da630c2e6"
    );
}

#[test]
fn roster_and_budget_validation_fail_closed() {
    for roster in [vec![], vec!["same", "same"], vec![""]] {
        assert!(CollectionWorkflowRecordV1::start(start("validation", "b1", &roster)).is_err());
    }
    let mut oversized = start("validation", "b2", &["a", "b", "c", "d"]);
    oversized.budgets.max_members = 3;
    assert!(CollectionWorkflowRecordV1::start(oversized).is_err());
    let mut invalid_budget = start("validation", "b3", &["a"]);
    invalid_budget.budgets.max_concurrency = 0;
    assert!(CollectionWorkflowRecordV1::start(invalid_budget).is_err());
}

#[test]
fn additive_reader_rejects_an_unsupported_intent_version() {
    let (intent, _) = CollectionWorkflowRecordV1::start(start("version-check", "b1", &["a"]))
        .expect("valid start");
    let mut payload = serde_json::json!({});
    attach_collection_start(&mut payload, &intent).expect("attach supported intent");
    payload[COLLECTION_START_INTENTS_FIELD][0]["version"] = serde_json::json!(2);
    let error = extract_collection_starts(&payload).expect_err("version 2 must fail closed");
    assert!(error.contains("unsupported collection ledger version 2"));

    let (second, _) = CollectionWorkflowRecordV1::start(start("version-check", "b2", &["b"]))
        .expect("second valid start");
    assert!(attach_collection_start(&mut payload, &second).is_err());
    let mut forged = serde_json::json!({});
    forged[COLLECTION_START_INTENTS_FIELD] = serde_json::json!([intent, second]);
    assert!(extract_collection_starts(&forged).is_err());
}

#[test]
fn duplicate_member_terminal_evidence_is_idempotent() {
    let (_, mut record) = CollectionWorkflowRecordV1::start(start("member-receipt", "b1", &["a"]))
        .expect("valid start");
    record
        .admit_member(0, "delivery-a".to_string(), 0)
        .expect("admit member");
    let receipt = CollectionMemberReceipt {
        delivery_id: "delivery-a".to_string(),
        fencing_token: 1,
    };
    record
        .record_member_receipt(
            &record.members[0].member_id.clone(),
            "delivery-a",
            0,
            1,
            receipt.clone(),
        )
        .expect("record target receipt");
    let evidence = CollectionMemberTerminalEvidence {
        member_id: record.members[0].member_id.clone(),
        control_epoch: 0,
        status: CollectionMemberStatus::Succeeded,
        attempts: 1,
        delivery_id: Some("delivery-a".to_string()),
        delivery_status: ReactionDeliveryStatus::Succeeded,
        receipt: Some(receipt),
        failure_class: None,
    };
    assert_eq!(
        record
            .record_member_terminal(evidence.clone())
            .expect("first receipt"),
        CollectionMutationOutcome::Applied
    );
    assert_eq!(
        record
            .record_member_terminal(evidence)
            .expect("duplicate receipt"),
        CollectionMutationOutcome::Replayed
    );
    assert_eq!(record.status, CollectionWorkflowStatus::Succeeded);
    assert_eq!(record.counts.succeeded, 1);
    assert_eq!(record.total_attempts, 1);
}

#[test]
fn terminal_evidence_requires_admission_delivery_and_epoch_fences() {
    let (_, mut record) =
        CollectionWorkflowRecordV1::start(start("member-fences", "b1", &["a", "b"]))
            .expect("valid start");
    let pending = CollectionMemberTerminalEvidence {
        member_id: record.members[0].member_id.clone(),
        control_epoch: 0,
        status: CollectionMemberStatus::Failed,
        attempts: 1,
        delivery_id: Some("not-admitted".to_string()),
        delivery_status: ReactionDeliveryStatus::Rejected,
        receipt: None,
        failure_class: Some(CollectionFailureClass::PermanentRejected),
    };
    assert!(record.record_member_terminal(pending).is_err());
    record
        .admit_member(0, "delivery-a".to_string(), 0)
        .expect("admit member");
    let wrong_delivery = CollectionMemberTerminalEvidence {
        member_id: record.members[0].member_id.clone(),
        control_epoch: 0,
        status: CollectionMemberStatus::Failed,
        attempts: 1,
        delivery_id: Some("delivery-b".to_string()),
        delivery_status: ReactionDeliveryStatus::Rejected,
        receipt: None,
        failure_class: Some(CollectionFailureClass::PermanentRejected),
    };
    assert!(record.record_member_terminal(wrong_delivery).is_err());
}

#[test]
fn replay_validation_derives_exact_lifecycle_and_member_shapes() {
    let (_, running) = CollectionWorkflowRecordV1::start(start("replay-validation", "b1", &["a"]))
        .expect("valid running record");
    let mut forged_terminal = running.clone();
    forged_terminal.status = CollectionWorkflowStatus::Succeeded;
    forged_terminal.terminal_classification = Some(CollectionWorkflowStatus::Succeeded);
    assert!(forged_terminal.validate().is_err());

    let mut malformed_pending = running.clone();
    malformed_pending.members[0].attempts = 1;
    malformed_pending.total_attempts = 1;
    assert!(malformed_pending.validate().is_err());

    let mut cancelled = running.clone();
    cancelled
        .request_control(
            CollectionRequestedOutcome::Cancelled,
            "CancelChecks".to_string(),
            2,
            serde_json::json!({"principal": "controller"}),
            None,
        )
        .expect("cancel workflow");
    assert_eq!(cancelled.status, CollectionWorkflowStatus::Cancelled);
    let mut wrong_outcome = cancelled.clone();
    wrong_outcome.status = CollectionWorkflowStatus::TimedOut;
    wrong_outcome.terminal_classification = Some(CollectionWorkflowStatus::TimedOut);
    assert!(wrong_outcome.validate().is_err());

    let mut forged_running = cancelled;
    forged_running.status = CollectionWorkflowStatus::Running;
    forged_running.terminal_classification = None;
    assert!(forged_running.validate().is_err());
}
