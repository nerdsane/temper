//! Focused Redis event-store regression group.

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
async fn listings_exclude_canonical_and_legacy_tombstones() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
    let tenant = format!("tombstone-listing-{}", uuid::Uuid::new_v4());
    let active = format!("{tenant}:Order:active");
    let canonical = format!("{tenant}:Order:canonical-deleted");
    let canonical_first = format!("{tenant}:Order:canonical-first-event-deleted");
    let legacy = format!("{tenant}:Order:legacy-deleted");
    let action_named_live = format!("{tenant}:Order:action-named-live");

    store
        .append(
            &active,
            0,
            &[test_envelope("Created", serde_json::json!({}))],
        )
        .await
        .unwrap();
    store
        .append(
            &canonical,
            0,
            &[
                test_envelope("Created", serde_json::json!({})),
                test_envelope("Deleted", serde_json::json!({})),
            ],
        )
        .await
        .unwrap();
    store
        .append(
            &legacy,
            0,
            &[
                test_envelope("Created", serde_json::json!({})),
                test_envelope(
                    "Delete",
                    serde_json::json!({
                        "action": "Delete",
                        "from_status": "Draft",
                        "to_status": "Deleted"
                    }),
                ),
            ],
        )
        .await
        .unwrap();
    store
        .append(
            &canonical_first,
            0,
            &[test_envelope("Deleted", serde_json::json!({}))],
        )
        .await
        .unwrap();
    store
        .append(
            &action_named_live,
            0,
            &[test_envelope(
                "Transitioned",
                serde_json::json!({
                    "action": "Deleted",
                    "from_status": "Draft",
                    "to_status": "Running"
                }),
            )],
        )
        .await
        .unwrap();

    assert_eq!(
        store.list_entity_ids(&tenant).await.unwrap(),
        vec![
            ("Order".to_string(), "action-named-live".to_string()),
            ("Order".to_string(), "active".to_string()),
        ]
    );
    assert_eq!(
        store
            .list_entity_ids_by_type(&tenant, "Order")
            .await
            .unwrap(),
        vec!["action-named-live".to_string(), "active".to_string()]
    );
    assert_eq!(
        store
            .list_entity_ids_limited(&tenant, Some("Order"), 10)
            .await
            .unwrap(),
        vec![
            ("Order".to_string(), "action-named-live".to_string()),
            ("Order".to_string(), "active".to_string()),
        ]
    );
}
