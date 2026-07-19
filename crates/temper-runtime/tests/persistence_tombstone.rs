use temper_runtime::persistence::{EventMetadata, PersistenceEnvelope};

fn envelope(event_type: &str, payload: serde_json::Value) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 1,
        event_type: event_type.to_string(),
        payload,
        metadata: EventMetadata {
            event_id: uuid::Uuid::nil(),
            causation_id: uuid::Uuid::nil(),
            correlation_id: uuid::Uuid::nil(),
            timestamp: chrono::DateTime::UNIX_EPOCH,
            actor_id: "default:Doc:doc-1".to_string(),
        },
    }
}

#[test]
fn explicit_live_target_status_outranks_legacy_deleted_names() {
    let explicitly_live = envelope(
        "Deleted",
        serde_json::json!({
            "action": "Deleted",
            "from_status": "Ready",
            "to_status": "Ready"
        }),
    );

    assert!(
        !explicitly_live.transitions_to_deleted(),
        "structured lifecycle metadata must outrank legacy event/action names"
    );
}

#[test]
fn legacy_deleted_name_remains_a_tombstone_without_target_status() {
    let legacy = envelope("Deleted", serde_json::json!({"action": "Deleted"}));

    assert!(legacy.transitions_to_deleted());
}
