use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};

use super::TursoEventStore;

fn envelope(event_type: &str, payload: serde_json::Value) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: event_type.to_string(),
        payload,
        metadata: EventMetadata {
            event_id: uuid::Uuid::new_v4(),
            causation_id: uuid::Uuid::new_v4(),
            correlation_id: uuid::Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
            actor_id: "listing-test".to_string(),
        },
    }
}

#[tokio::test]
async fn listings_exclude_legacy_payload_tombstones() {
    let path = std::env::temp_dir().join(format!(
        "temper-store-turso-legacy-tombstone-{}.db",
        uuid::Uuid::new_v4()
    ));
    let store = TursoEventStore::new(&format!("file:{}", path.display()), None)
        .await
        .expect("create local store");
    store
        .append(
            "tenant-a:Order:deleted",
            0,
            &[
                envelope("Created", serde_json::json!({})),
                envelope(
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
        .expect("append legacy tombstone");
    store
        .append(
            "tenant-a:Order:active",
            0,
            &[envelope("Created", serde_json::json!({}))],
        )
        .await
        .expect("append active entity");
    store
        .append(
            "tenant-a:Order:action-named-live",
            0,
            &[envelope(
                "Transitioned",
                serde_json::json!({
                    "action": "Deleted",
                    "from_status": "Draft",
                    "to_status": "Running"
                }),
            )],
        )
        .await
        .expect("append live transition whose action label is Deleted");

    assert_eq!(
        store
            .list_entity_ids("tenant-a")
            .await
            .expect("list tenant"),
        vec![
            ("Order".to_string(), "action-named-live".to_string()),
            ("Order".to_string(), "active".to_string()),
        ]
    );
    assert_eq!(
        store
            .list_entity_ids_by_type("tenant-a", "Order")
            .await
            .expect("list type"),
        vec!["action-named-live".to_string(), "active".to_string()]
    );
    assert_eq!(
        store
            .list_entity_ids_limited("tenant-a", None, 10)
            .await
            .expect("list limited"),
        vec![
            ("Order".to_string(), "action-named-live".to_string()),
            ("Order".to_string(), "active".to_string()),
        ]
    );
}
