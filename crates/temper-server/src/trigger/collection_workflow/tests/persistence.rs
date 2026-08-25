use super::*;

#[tokio::test]
async fn atomic_helpers_reject_mismatched_source_and_intent_evidence() {
    let store = BoxedEventStore::new(temper_store_sim::SimEventStore::no_faults(410));
    let (intent, record) =
        CollectionWorkflowRecordV1::start(start("evidence-fences", "batch-1", &["a"]))
            .expect("valid start");

    let wrong_journal = source_append("evidence-fences", "other-batch", 0, "StartChecks");
    assert!(matches!(
        commit_collection_start(&store, wrong_journal, &intent, &record).await,
        Err(PersistenceError::Serialization(_))
    ));
    let wrong_action = source_append("evidence-fences", "batch-1", 0, "UnrelatedAction");
    assert!(matches!(
        commit_collection_start(&store, wrong_action, &intent, &record).await,
        Err(PersistenceError::Serialization(_))
    ));
    let mut contradictory = intent.clone();
    contradictory.start.roster = vec!["different".to_string()];
    assert!(matches!(
        commit_collection_start(
            &store,
            source_append("evidence-fences", "batch-1", 0, "StartChecks"),
            &contradictory,
            &record,
        )
        .await,
        Err(PersistenceError::Serialization(_))
    ));
    assert!(
        store
            .read_events("evidence-fences:Batch:batch-1", 0)
            .await
            .expect("read rejected starts")
            .is_empty()
    );

    commit_collection_start(
        &store,
        source_append("evidence-fences", "batch-1", 0, "StartChecks"),
        &intent,
        &record,
    )
    .await
    .expect("commit valid start");
    let mut controlled = record.clone();
    let (control, _) = controlled
        .request_control(
            CollectionRequestedOutcome::Cancelled,
            None,
            "CancelChecks".to_string(),
            2,
            serde_json::json!({"principal": "controller"}),
            None,
        )
        .expect("request control");
    let mut contradictory_control = control.clone();
    contradictory_control.authority = serde_json::json!({"principal": "other"});
    assert!(matches!(
        commit_collection_control(
            &store,
            source_append("evidence-fences", "batch-1", 1, "CancelChecks"),
            &contradictory_control,
            1,
            &controlled,
        )
        .await,
        Err(PersistenceError::Serialization(_))
    ));
    assert!(matches!(
        commit_collection_control(
            &store,
            source_append("evidence-fences", "batch-1", 1, "UnrelatedAction"),
            &control,
            1,
            &controlled,
        )
        .await,
        Err(PersistenceError::Serialization(_))
    ));
}
