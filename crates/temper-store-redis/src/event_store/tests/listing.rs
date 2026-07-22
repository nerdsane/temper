//! listing scenarios.

use super::*;

#[tokio::test]
async fn list_entity_ids_returns_distinct_pairs() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
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
async fn concurrent_appends_detect_conflict() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
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
