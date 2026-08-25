use super::*;

async fn prove_collection_ledger_semantics(tenant: &str, store: BoxedEventStore) {
    let (intent, mut record) =
        CollectionWorkflowRecordV1::start(start(tenant, "batch-1", &["a", "b", "c"]))
            .expect("valid workflow start");
    let source = source_append(tenant, "batch-1", 0, "StartChecks");
    assert!(matches!(
        commit_collection_start(&store, source.clone(), &intent, &record)
            .await
            .expect("commit start"),
        CollectionLedgerCommitOutcome::Committed(_)
    ));
    assert!(matches!(
        commit_collection_start(&store, source, &intent, &record)
            .await
            .expect("reconcile duplicate start"),
        CollectionLedgerCommitOutcome::Reconciled(_)
    ));

    let loaded = load_collection_record(&store, tenant, &record.workflow_id)
        .await
        .expect("load start")
        .expect("workflow exists");
    assert_eq!(loaded, (record.clone(), 1));

    record
        .admit_member(0, "delivery-a".to_string(), 0)
        .expect("admit first member");
    assert_eq!(
        append_collection_record_idempotent(&store, 1, "CollectionWorkflow::AdmittedV1", &record)
            .await
            .expect("append admission"),
        (CollectionMutationOutcome::Applied, 2)
    );
    assert_eq!(
        append_collection_record_idempotent(&store, 1, "CollectionWorkflow::AdmittedV1", &record)
            .await
            .expect("reconcile admission"),
        (CollectionMutationOutcome::Replayed, 2)
    );

    let receipt = CollectionMemberReceipt {
        delivery_id: "delivery-a".to_string(),
        fencing_token: 7,
    };
    assert_eq!(
        record
            .record_member_receipt(
                &record.members[0].member_id.clone(),
                "delivery-a",
                0,
                1,
                receipt,
            )
            .expect("record receipt before control"),
        CollectionMutationOutcome::Applied
    );
    assert_eq!(
        append_collection_record_idempotent(&store, 2, "CollectionWorkflow::ReceiptedV1", &record)
            .await
            .expect("append receipt"),
        (CollectionMutationOutcome::Applied, 3)
    );
    assert_eq!(
        append_collection_record_idempotent(&store, 1, "CollectionWorkflow::AdmittedV1", &{
            let mut admitted = record.clone();
            admitted.members[0].attempts = 0;
            admitted.members[0].receipt = None;
            admitted.members[0].delivery_status = Some(ReactionDeliveryStatus::Pending);
            admitted.total_attempts = 0;
            admitted
        })
        .await
        .expect("reconcile admission after later workflow progress"),
        (CollectionMutationOutcome::Replayed, 2)
    );
    assert!(matches!(
        commit_collection_start(
            &store,
            source_append(tenant, "batch-1", 0, "StartChecks"),
            &intent,
            &{
                let (_, original) =
                    CollectionWorkflowRecordV1::start(start(tenant, "batch-1", &["a", "b", "c"]))
                        .expect("rebuild start record");
                original
            }
        )
        .await
        .expect("reconcile start after later workflow progress"),
        CollectionLedgerCommitOutcome::Reconciled(_)
    ));

    let (control, control_outcome) = record
        .request_control(
            CollectionRequestedOutcome::Cancelled,
            None,
            "CancelChecks".to_string(),
            2,
            serde_json::json!({"principal": "controller"}),
            None,
        )
        .expect("request control");
    assert_eq!(control_outcome, CollectionMutationOutcome::Applied);
    let control_source = source_append(tenant, "batch-1", 1, "CancelChecks");
    assert!(matches!(
        commit_collection_control(&store, control_source.clone(), &control, 3, &record)
            .await
            .expect("commit control"),
        CollectionLedgerCommitOutcome::Committed(_)
    ));
    assert!(matches!(
        commit_collection_control(&store, control_source, &control, 3, &record)
            .await
            .expect("reconcile duplicate control"),
        CollectionLedgerCommitOutcome::Reconciled(_)
    ));
    assert_eq!(
        append_collection_record_idempotent(&store, 4, "CollectionWorkflow::AuditedV1", &record)
            .await
            .expect("append later lifecycle snapshot"),
        (CollectionMutationOutcome::Applied, 5)
    );
    assert!(matches!(
        commit_collection_control(
            &store,
            source_append(tenant, "batch-1", 1, "CancelChecks"),
            &control,
            3,
            &record,
        )
        .await
        .expect("reconcile control after later workflow progress"),
        CollectionLedgerCommitOutcome::Reconciled(_)
    ));

    let restarted = load_collection_record(&store, tenant, &record.workflow_id)
        .await
        .expect("reload after control")
        .expect("workflow exists after restart");
    assert_eq!(restarted, (record.clone(), 5));
    assert_eq!(record.next_undispatched_index, 3);
    assert_eq!(record.counts.in_flight, 1);
    assert_eq!(record.counts.cancelled, 2);
    assert_eq!(
        record.requested_outcome,
        Some(CollectionRequestedOutcome::Cancelled)
    );

    let pages = list_collection_records_page(&store, tenant, None, 1)
        .await
        .expect("bounded workflow page");
    assert_eq!(pages, vec![(record.clone(), 5)]);

    let (conflict_intent, conflict_record) =
        CollectionWorkflowRecordV1::start(start(tenant, "batch-conflict", &["only"]))
            .expect("valid conflict fixture");
    append_collection_record_idempotent(
        &store,
        0,
        "CollectionWorkflow::PreexistingV1",
        &conflict_record,
    )
    .await
    .expect("seed conflicting workflow journal");
    let conflict_source = source_append(tenant, "batch-conflict", 0, "StartChecks");
    assert!(matches!(
        commit_collection_start(&store, conflict_source, &conflict_intent, &conflict_record).await,
        Err(PersistenceError::ConcurrencyViolation { .. })
    ));
    assert!(
        store
            .read_events(&format!("{tenant}:Batch:batch-conflict"), 0)
            .await
            .expect("read conflict source")
            .is_empty(),
        "a failed batch must not commit only the source side"
    );
}

#[tokio::test]
async fn sim_matches_collection_ledger_semantics() {
    let store = temper_store_sim::SimEventStore::no_faults(41);
    prove_collection_ledger_semantics("collection-sim-parity", BoxedEventStore::new(store)).await;
}

#[tokio::test]
async fn turso_matches_collection_ledger_semantics() {
    let dir = tempfile::tempdir().expect("tempdir");
    let url = format!("file:{}", dir.path().join("collection.db").display());
    let store = temper_store_turso::TursoEventStore::new(&url, None)
        .await
        .expect("create Turso store");
    prove_collection_ledger_semantics("collection-turso-parity", BoxedEventStore::new(store)).await;
}

#[tokio::test]
async fn postgres_matches_collection_ledger_semantics_when_available() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        assert_ne!(
            std::env::var("TEMPER_REQUIRE_BACKEND_PARITY").as_deref(),
            Ok("1"),
            "DATABASE_URL is required by the backend parity CI gate"
        );
        return;
    };
    let pool = sqlx::PgPool::connect(&database_url)
        .await
        .expect("connect Postgres");
    temper_store_postgres::migration::run_migrations(&pool)
        .await
        .expect("run Postgres migrations");
    let tenant = format!("collection-postgres-{}", uuid::Uuid::new_v4());
    let store = temper_store_postgres::PostgresEventStore::new(pool);
    prove_collection_ledger_semantics(&tenant, BoxedEventStore::new(store)).await;
}

#[tokio::test]
async fn redis_matches_collection_ledger_semantics_when_available() {
    let Ok(redis_url) = std::env::var("REDIS_URL") else {
        assert_ne!(
            std::env::var("TEMPER_REQUIRE_BACKEND_PARITY").as_deref(),
            Ok("1"),
            "REDIS_URL is required by the backend parity CI gate"
        );
        return;
    };
    let tenant = format!("collection-redis-{}", uuid::Uuid::new_v4());
    let store = temper_store_redis::RedisEventStore::new(&redis_url)
        .await
        .expect("connect Redis");
    prove_collection_ledger_semantics(&tenant, BoxedEventStore::new(store)).await;
}
