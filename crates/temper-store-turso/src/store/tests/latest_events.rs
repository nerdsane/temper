use super::*;
use temper_runtime::persistence::LATEST_EVENT_BATCH_SIZE;

#[tokio::test]
async fn bounded_latest_event_read_preserves_order_and_reads_only_the_tail() {
    let store = make_store("latest-events-order").await;
    let first = "tenant-a:Order:first";
    let second = "tenant-a:Order:second";
    store
        .append(
            first,
            0,
            &[
                test_envelope("Created", serde_json::json!({ "version": 1 })),
                test_envelope("Updated", serde_json::json!({ "version": 2 })),
                test_envelope("Updated", serde_json::json!({ "version": 3 })),
            ],
        )
        .await
        .unwrap();
    store
        .append(
            second,
            0,
            &[test_envelope(
                "Created",
                serde_json::json!({ "version": 9 }),
            )],
        )
        .await
        .unwrap();

    let latest = store
        .read_latest_events(&[
            second.to_string(),
            "tenant-a:Order:missing".to_string(),
            first.to_string(),
        ])
        .await
        .unwrap();

    assert_eq!(latest.len(), 3);
    assert_eq!(latest[0].as_ref().unwrap().sequence_nr, 1);
    assert!(latest[1].is_none());
    assert_eq!(latest[2].as_ref().unwrap().sequence_nr, 3);
    assert_eq!(latest[2].as_ref().unwrap().payload["version"], 3);
}

#[tokio::test]
async fn corrupt_latest_event_fails_the_complete_batch() {
    let store = make_store("latest-events-corrupt").await;
    let persistence_id = "tenant-a:Order:corrupt";
    store
        .append(
            persistence_id,
            0,
            &[test_envelope("Created", serde_json::json!({}))],
        )
        .await
        .unwrap();

    store
        .connection()
        .unwrap()
        .execute(
            "UPDATE events SET metadata = 'not-json'
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
            params!["tenant-a", "Order", "corrupt"],
        )
        .await
        .unwrap();

    let result = store
        .read_latest_events(&[persistence_id.to_string()])
        .await;
    assert!(matches!(result, Err(PersistenceError::Serialization(_))));
}

#[tokio::test]
async fn latest_event_batch_budget_is_enforced() {
    let persistence_ids = (0..=LATEST_EVENT_BATCH_SIZE)
        .map(|index| format!("tenant-a:Order:{index}"))
        .collect::<Vec<_>>();
    let store = make_store("latest-events-budget").await;

    let result = store.read_latest_events(&persistence_ids).await;
    assert!(matches!(result, Err(PersistenceError::Storage(_))));
}
