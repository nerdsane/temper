use super::*;

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
            actor_id: "test".to_string(),
        },
    }
}

#[test]
fn canonical_tombstone_predicate_accepts_all_lifecycle_forms() {
    assert!(is_deletion_tombstone(&envelope(
        "Deleted",
        serde_json::json!({})
    )));
    assert!(is_deletion_tombstone(&envelope(
        "EntityEvent",
        serde_json::json!({ "action": "Deleted" })
    )));
    assert!(is_deletion_tombstone(&envelope(
        "Delete",
        serde_json::json!({ "action": "Delete", "to_status": "Deleted" })
    )));
    assert!(!is_deletion_tombstone(&envelope(
        "Created",
        serde_json::json!({ "action": "Created" })
    )));

    let deleted_with_audit = [
        envelope("Deleted", serde_json::json!({})),
        envelope(COMPOSITE_EVENT_TYPE, serde_json::json!({})),
    ];
    assert!(contains_deletion_tombstone(&deleted_with_audit));
    assert!(ends_in_deletion_tombstone(&deleted_with_audit));

    let recreated_with_audit = [
        envelope("Deleted", serde_json::json!({})),
        envelope("Created", serde_json::json!({ "action": "Created" })),
        envelope(COMPOSITE_EVENT_TYPE, serde_json::json!({})),
    ];
    assert!(contains_deletion_tombstone(&recreated_with_audit));
    assert!(!ends_in_deletion_tombstone(&recreated_with_audit));
}

#[test]
fn latest_event_batch_budget_rejects_oversized_requests() {
    let at_budget = vec!["tenant:Type:id".to_string(); LATEST_EVENT_BATCH_SIZE];
    assert!(validate_latest_event_batch(&at_budget).is_ok());

    let over_budget = vec!["tenant:Type:id".to_string(); LATEST_EVENT_BATCH_SIZE + 1];
    assert!(validate_latest_event_batch(&over_budget).is_err());
}

#[test]
fn append_batch_validation_rejects_duplicate_streams() {
    let duplicate = PersistenceAppend {
        persistence_id: "tenant:Order:duplicate".to_string(),
        expected_sequence: 0,
        events: Vec::new(),
        key_rows: None,
    };
    assert!(
        validate_persistence_append_batch(&[duplicate.clone(), duplicate]).is_err(),
        "even empty members must not make duplicate-stream batch results ambiguous"
    );

    let legacy = PersistenceAppend {
        persistence_id: "Order:aliased".to_string(),
        expected_sequence: 0,
        events: Vec::new(),
        key_rows: None,
    };
    let qualified = PersistenceAppend {
        persistence_id: "default:Order:aliased".to_string(),
        ..legacy.clone()
    };
    assert!(
        validate_persistence_append_batch(&[legacy, qualified]).is_err(),
        "legacy and qualified ids for one physical stream are duplicates"
    );
}

#[test]
fn guarded_batch_validation_rejects_guard_aliasing_append() {
    let append = PersistenceAppend {
        persistence_id: "Widget:w1".to_string(),
        expected_sequence: 0,
        events: vec![envelope("Created", serde_json::json!({}))],
        key_rows: None,
    };
    let guard = PersistenceSequenceGuard {
        persistence_id: "default:Widget:w1".to_string(),
        expected_sequence: 0,
    };
    assert!(validate_guarded_persistence_append_batch(&[append], &[guard]).is_err());
}

#[test]
fn batch_key_intent_deserialization_preserves_omitted_and_explicit_empty_modes() {
    let omitted: PersistenceAppend = serde_json::from_value(serde_json::json!({
        "persistence_id": "tenant:Order:raw",
        "expected_sequence": 0,
        "events": []
    }))
    .unwrap();
    assert_eq!(omitted.key_rows, None);

    let explicit_empty: PersistenceAppend = serde_json::from_value(serde_json::json!({
        "persistence_id": "tenant:Order:governed",
        "expected_sequence": 0,
        "events": [],
        "key_rows": []
    }))
    .unwrap();
    assert_eq!(explicit_empty.key_rows, Some(Vec::new()));
}
