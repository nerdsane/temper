use super::*;

#[tokio::test]
async fn list_entity_ids_returns_distinct_pairs() {
    let store = make_store("entity-list").await;

    let tenant_a = format!("tenant-a-{}", uuid::Uuid::new_v4());
    let tenant_b = format!("tenant-b-{}", uuid::Uuid::new_v4());

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
            &order_1,
            1,
            &[test_envelope(
                "OrderUpdated",
                serde_json::json!({ "step": 2 }),
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
}

#[tokio::test]
async fn list_entity_ids_by_type_uses_entity_catalog() {
    let store = make_store("entity-list-by-type-catalog").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());

    store
        .upsert_query_projection(
            &tenant,
            "AgentRoute",
            "route-main",
            "Ready",
            &serde_json::json!({ "Name": "main" }),
            3,
        )
        .await
        .expect("upsert AgentRoute projection");
    store
        .upsert_query_projection(
            &tenant,
            "Session",
            "session-1",
            "Completed",
            &serde_json::json!({ "Name": "session" }),
            1,
        )
        .await
        .expect("upsert Session projection");

    let ids = store
        .list_entity_ids_by_type(&tenant, "AgentRoute")
        .await
        .expect("list AgentRoute IDs by type");

    assert_eq!(ids, vec!["route-main".to_string()]);
}

#[tokio::test]
async fn list_entity_ids_by_type_unions_catalog_field_index_and_events() {
    let store = make_store("entity-list-by-type-union").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());

    store
        .upsert_query_projection(
            &tenant,
            "AgentRoute",
            "route-catalog",
            "Ready",
            &serde_json::json!({ "Name": "catalog" }),
            3,
        )
        .await
        .expect("upsert catalog projection");
    store
        .upsert_query_projection(
            &tenant,
            "AgentRoute",
            "route-deleted",
            "Ready",
            &serde_json::json!({ "Name": "deleted" }),
            3,
        )
        .await
        .expect("upsert deleted projection");

    let conn = store.connection().expect("connection");
    conn.execute(
        "INSERT INTO entity_field_index \
         (tenant, entity_type, entity_id, field_name, field_value, status) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            tenant.clone(),
            "AgentRoute",
            "route-index",
            "Name",
            "index",
            "Ready"
        ],
    )
    .await
    .expect("insert field-index-only row");

    store
        .append(
            &format!("{tenant}:AgentRoute:route-event"),
            0,
            &[test_envelope("Created", serde_json::json!({}))],
        )
        .await
        .expect("append event-only route");
    store
        .append(
            &format!("{tenant}:AgentRoute:route-deleted"),
            0,
            &[test_envelope("Deleted", serde_json::json!({}))],
        )
        .await
        .expect("append deleted tombstone");

    let ids = store
        .list_entity_ids_by_type(&tenant, "AgentRoute")
        .await
        .expect("list AgentRoute IDs by type");

    assert_eq!(
        ids,
        vec![
            "route-catalog".to_string(),
            "route-event".to_string(),
            "route-index".to_string(),
        ]
    );
}

#[tokio::test]
async fn list_entity_ids_by_type_includes_events_and_excludes_deleted() {
    let store = make_store("entity-list-by-type-events").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());

    let deleted_order = format!("{tenant}:Order:ord-deleted");
    let active_order = format!("{tenant}:Order:ord-active");
    let task = format!("{tenant}:Task:task-1");

    store
        .append(
            &deleted_order,
            0,
            &[test_envelope("Created", serde_json::json!({}))],
        )
        .await
        .unwrap();
    store
        .append(
            &deleted_order,
            1,
            &[test_envelope("Deleted", serde_json::json!({}))],
        )
        .await
        .unwrap();
    store
        .append(
            &active_order,
            0,
            &[test_envelope("Created", serde_json::json!({}))],
        )
        .await
        .unwrap();
    store
        .append(&task, 0, &[test_envelope("Created", serde_json::json!({}))])
        .await
        .unwrap();

    let ids = store
        .list_entity_ids_by_type(&tenant, "Order")
        .await
        .expect("list Order IDs by type from events");

    assert_eq!(ids, vec!["ord-active".to_string()]);
}

#[tokio::test]
async fn list_entity_ids_excludes_entities_with_deleted_tombstones() {
    let store = make_store("entity-list-deleted").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());

    let deleted_order = format!("{tenant}:Order:ord-deleted");
    let active_order = format!("{tenant}:Order:ord-active");

    store
        .append(
            &deleted_order,
            0,
            &[test_envelope(
                "Created",
                serde_json::json!({ "id": "ord-deleted" }),
            )],
        )
        .await
        .unwrap();
    store
        .append(
            &deleted_order,
            1,
            &[test_envelope(
                "Deleted",
                serde_json::json!({
                    "action": "Deleted",
                    "from_status": "Draft",
                    "to_status": "Deleted"
                }),
            )],
        )
        .await
        .unwrap();
    store
        .append(
            &active_order,
            0,
            &[test_envelope(
                "Created",
                serde_json::json!({ "id": "ord-active" }),
            )],
        )
        .await
        .unwrap();

    let mut entities = store.list_entity_ids(&tenant).await.unwrap();
    entities.sort();

    assert_eq!(
        entities,
        vec![("Order".to_string(), "ord-active".to_string())]
    );
}
