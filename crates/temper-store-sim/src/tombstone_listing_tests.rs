use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::{install_deterministic_context, sim_now, sim_uuid};

use crate::SimEventStore;

fn envelope(event_type: &str, payload: serde_json::Value) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: event_type.to_string(),
        payload,
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: "listing-test".to_string(),
        },
    }
}

#[tokio::test]
async fn listings_exclude_legacy_payload_tombstones() {
    let (_guard, _clock, _ids) = install_deterministic_context(249);
    let store = SimEventStore::no_faults(249);
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
}
