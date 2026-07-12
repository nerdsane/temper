use super::*;
use temper_runtime::persistence::EventMetadata;

fn test_envelope(event_type: &str, payload: serde_json::Value) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: event_type.to_string(),
        payload,
        metadata: EventMetadata {
            event_id: uuid::Uuid::new_v4(),
            causation_id: uuid::Uuid::new_v4(),
            correlation_id: uuid::Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
            actor_id: "redis-test".to_string(),
        },
    }
}

fn unique_persistence_id() -> String {
    let id = uuid::Uuid::new_v4();
    format!("test-{id}:Order:ord-{id}")
}

async fn make_store() -> RedisEventStore {
    let url = std::env::var("REDIS_URL").expect("REDIS_URL for ignored Redis integration test");
    RedisEventStore::new(&url)
        .await
        .expect("failed to connect to Redis")
}

#[tokio::test]
#[ignore = "requires REDIS_URL and a live Redis service"]
async fn append_and_read_events_roundtrip() {
    let store = make_store().await;
    let pid = unique_persistence_id();

    let new_seq = store
        .append(
            &pid,
            0,
            &[
                test_envelope("OrderCreated", serde_json::json!({ "id": "ord-1" })),
                test_envelope("OrderApproved", serde_json::json!({ "approved": true })),
            ],
        )
        .await
        .unwrap();

    assert_eq!(new_seq, 2);

    // Read all events
    let events = store.read_events(&pid, 0).await.unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].sequence_nr, 1);
    assert_eq!(events[1].sequence_nr, 2);
    assert_eq!(events[0].event_type, "OrderCreated");
    assert_eq!(events[1].event_type, "OrderApproved");

    // Partial read (from_sequence = 1 should skip event 1)
    let partial = store.read_events(&pid, 1).await.unwrap();
    assert_eq!(partial.len(), 1);
    assert_eq!(partial[0].sequence_nr, 2);
    assert_eq!(partial[0].event_type, "OrderApproved");

    assert_eq!(
        store.read_events_bounded(&pid, 0, 1).await.unwrap()[0].sequence_nr,
        1
    );
}

#[tokio::test]
#[ignore = "requires REDIS_URL and a live Redis service"]
async fn empty_append_does_not_create_a_discovery_entry() {
    let store = make_store().await;
    let pid = unique_persistence_id();
    let (tenant, _, _) = parse_persistence_id_parts(&pid).unwrap();

    assert_eq!(store.append(&pid, 9, &[]).await.unwrap(), 9);
    assert!(store.list_entity_ids(tenant).await.unwrap().is_empty());
    assert!(
        store
            .list_entity_ids_limited(tenant, None, 1)
            .await
            .unwrap()
            .is_empty()
    );

    let second = format!("{tenant}:Order:empty-second");
    let results = store
        .append_batch(&[
            PersistenceAppend {
                persistence_id: pid.clone(),
                expected_sequence: 9,
                events: vec![],
                key_rows: None,
                vector_rows: Vec::new(),
                reconcile_vectors: false,
            },
            PersistenceAppend {
                persistence_id: second,
                expected_sequence: 4,
                events: vec![],
                key_rows: None,
                vector_rows: Vec::new(),
                reconcile_vectors: false,
            },
        ])
        .await
        .unwrap();
    assert_eq!(
        results
            .into_iter()
            .map(|result| result.sequence_nr)
            .collect::<Vec<_>>(),
        vec![9, 4]
    );

    let written = format!("{tenant}:Order:written");
    let mixed = store
        .append_batch(&[
            PersistenceAppend {
                persistence_id: pid,
                expected_sequence: 9,
                events: vec![],
                key_rows: None,
                vector_rows: Vec::new(),
                reconcile_vectors: false,
            },
            PersistenceAppend {
                persistence_id: written.clone(),
                expected_sequence: 0,
                events: vec![test_envelope("Created", serde_json::json!({}))],
                key_rows: None,
                vector_rows: Vec::new(),
                reconcile_vectors: false,
            },
        ])
        .await
        .unwrap();
    assert_eq!(
        mixed
            .into_iter()
            .map(|result| result.sequence_nr)
            .collect::<Vec<_>>(),
        vec![9, 1]
    );
    assert_eq!(store.read_events(&written, 0).await.unwrap().len(), 1);
}

#[tokio::test]
#[ignore = "requires REDIS_URL and a live Redis service"]
async fn limited_listing_is_ordered_bounded_and_migrates_legacy_members() {
    let store = make_store().await;
    let tenant = format!("limited-{}", uuid::Uuid::new_v4());
    for (entity_type, entity_id) in [("Task", "task-1"), ("Order", "ord-2")] {
        store
            .append(
                &format!("{tenant}:{entity_type}:{entity_id}"),
                0,
                &[test_envelope("Created", serde_json::json!({}))],
            )
            .await
            .unwrap();
    }

    let legacy = serde_json::to_string(&EntityRef {
        entity_type: "Order".to_string(),
        entity_id: "ord-1".to_string(),
    })
    .unwrap();
    let _: i64 = store
        .client
        .sadd(RedisEventStore::tenant_entities_key(&tenant), legacy)
        .await
        .unwrap();

    assert_eq!(
        store
            .list_entity_ids_limited(&tenant, None, 2)
            .await
            .unwrap(),
        vec![
            ("Order".to_string(), "ord-1".to_string()),
            ("Order".to_string(), "ord-2".to_string()),
        ]
    );
    assert_eq!(
        store
            .list_entity_ids_limited(&tenant, Some("Task"), 1)
            .await
            .unwrap(),
        vec![("Task".to_string(), "task-1".to_string())]
    );
}

#[tokio::test]
#[ignore = "requires REDIS_URL and a live Redis service"]
async fn migrated_ordered_index_repairs_legacy_writer_drift() {
    let store = make_store().await;
    let tenant = format!("limited-drift-{}", uuid::Uuid::new_v4());
    store
        .append(
            &format!("{tenant}:Task:task-initial"),
            0,
            &[test_envelope("Created", serde_json::json!({}))],
        )
        .await
        .unwrap();

    // Establish the migration marker and ordered indexes first.
    assert_eq!(
        store
            .list_entity_ids_limited(&tenant, None, 10)
            .await
            .unwrap(),
        vec![("Task".to_string(), "task-initial".to_string())]
    );

    // Simulate an older rolling-deploy writer that still updates only the
    // legacy SET after the marker exists. Cardinality drift must force a
    // bounded-memory remigration and repair both ordered indexes.
    let legacy = serde_json::to_string(&EntityRef {
        entity_type: "Order".to_string(),
        entity_id: "ord-legacy-after-marker".to_string(),
    })
    .unwrap();
    let _: i64 = store
        .client
        .sadd(RedisEventStore::tenant_entities_key(&tenant), legacy)
        .await
        .unwrap();

    let all = store
        .list_entity_ids_limited(&tenant, None, 10)
        .await
        .unwrap();
    assert!(all.contains(&("Order".to_string(), "ord-legacy-after-marker".to_string())));
    assert_eq!(
        store
            .list_entity_ids_limited(&tenant, Some("Order"), 10)
            .await
            .unwrap(),
        vec![("Order".to_string(), "ord-legacy-after-marker".to_string())]
    );
}

#[tokio::test]
#[ignore = "requires REDIS_URL and a live Redis service"]
async fn append_with_wrong_sequence_fails() {
    let store = make_store().await;
    let pid = unique_persistence_id();

    store
        .append(
            &pid,
            0,
            &[test_envelope(
                "OrderCreated",
                serde_json::json!({ "id": "ord-1" }),
            )],
        )
        .await
        .unwrap();

    let err = store
        .append(
            &pid,
            0, // stale: actual is 1
            &[test_envelope(
                "OrderUpdated",
                serde_json::json!({ "step": 2 }),
            )],
        )
        .await
        .unwrap_err();

    assert!(matches!(
        err,
        PersistenceError::ConcurrencyViolation {
            expected: 0,
            actual: 1
        }
    ));
}

#[tokio::test]
#[ignore = "requires REDIS_URL and a live Redis service"]
async fn snapshot_save_and_load_roundtrip() {
    let store = make_store().await;
    let pid = unique_persistence_id();

    store
        .save_snapshot(&pid, 5, b"{\"status\":\"created\"}")
        .await
        .unwrap();

    let snapshot = store.load_snapshot(&pid).await.unwrap();
    assert_eq!(snapshot, Some((5, b"{\"status\":\"created\"}".to_vec())));

    // Overwrite
    store
        .save_snapshot(&pid, 8, b"{\"status\":\"shipped\"}")
        .await
        .unwrap();

    let updated = store.load_snapshot(&pid).await.unwrap();
    assert_eq!(updated, Some((8, b"{\"status\":\"shipped\"}".to_vec())));

    store
        .save_snapshot(&pid, 3, b"stale-snapshot")
        .await
        .unwrap();
    assert_eq!(
        store.load_snapshot(&pid).await.unwrap(),
        Some((8, b"{\"status\":\"shipped\"}".to_vec())),
        "a delayed snapshot writer must not regress the recovery boundary"
    );

    let (tenant, entity_type, entity_id) = parse_persistence_id_parts(&pid).unwrap();
    let history_key = RedisEventStore::snapshot_history_key(tenant, entity_type, entity_id, 3);
    let history: Option<String> = store.client.get(history_key).await.unwrap();
    assert!(
        history.is_some(),
        "stale snapshots remain available in history"
    );
}

#[tokio::test]
#[ignore = "requires REDIS_URL and a live Redis service"]
async fn list_entity_ids_returns_distinct_pairs() {
    let store = make_store().await;
    let unique = uuid::Uuid::new_v4();
    let tenant_a = format!("tenant-a-{unique}");
    let tenant_b = format!("tenant-b-{unique}");

    let order_1 = format!("{tenant_a}:Order:ord-1");
    let order_2 = format!("{tenant_a}:Order:ord-2");
    let task_1 = format!("{tenant_a}:Task:task-1");
    let other_tenant = format!("{tenant_b}:Order:ord-9");

    store
        .append(
            &order_1,
            0,
            &[test_envelope(
                "OrderCreated",
                serde_json::json!({ "id": "ord-1" }),
            )],
        )
        .await
        .unwrap();
    store
        .append(
            &order_2,
            0,
            &[test_envelope(
                "OrderCreated",
                serde_json::json!({ "id": "ord-2" }),
            )],
        )
        .await
        .unwrap();
    store
        .append(
            &task_1,
            0,
            &[test_envelope(
                "TaskCreated",
                serde_json::json!({ "id": "task-1" }),
            )],
        )
        .await
        .unwrap();
    store
        .append(
            &other_tenant,
            0,
            &[test_envelope(
                "OrderCreated",
                serde_json::json!({ "id": "ord-9" }),
            )],
        )
        .await
        .unwrap();

    let mut entities = store.list_entity_ids(&tenant_a).await.unwrap();
    entities.sort();

    assert_eq!(
        entities,
        vec![
            ("Order".to_string(), "ord-1".to_string()),
            ("Order".to_string(), "ord-2".to_string()),
            ("Task".to_string(), "task-1".to_string()),
        ]
    );

    // Cross-tenant isolation
    let other_entities = store.list_entity_ids(&tenant_b).await.unwrap();
    assert_eq!(
        other_entities,
        vec![("Order".to_string(), "ord-9".to_string())]
    );
}

#[tokio::test]
#[ignore = "requires REDIS_URL and a live Redis service"]
async fn concurrent_appends_detect_conflict() {
    let store = make_store().await;
    let pid = unique_persistence_id();

    let store1 = store.clone();
    let store2 = store.clone();
    let pid1 = pid.clone();
    let pid2 = pid.clone();

    let handle1 = tokio::spawn(async move {
        store1
            .append(
                &pid1,
                0,
                &[test_envelope(
                    "OrderCreated",
                    serde_json::json!({ "writer": 1 }),
                )],
            )
            .await
    });

    let handle2 = tokio::spawn(async move {
        store2
            .append(
                &pid2,
                0,
                &[test_envelope(
                    "OrderCreated",
                    serde_json::json!({ "writer": 2 }),
                )],
            )
            .await
    });

    let (r1, r2) = tokio::join!(handle1, handle2);
    let r1 = r1.unwrap();
    let r2 = r2.unwrap();

    // Exactly one should succeed, the other should get a ConcurrencyViolation.
    let successes = [r1.is_ok(), r2.is_ok()].iter().filter(|&&ok| ok).count();
    let conflicts = [&r1, &r2]
        .iter()
        .filter(|r| matches!(r, Err(PersistenceError::ConcurrencyViolation { .. })))
        .count();

    assert_eq!(successes, 1, "exactly one writer should succeed");
    assert_eq!(conflicts, 1, "exactly one writer should see a conflict");
}
