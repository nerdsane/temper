//! Regression for snapshot baseline changes after key coverage publication.

use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_store_sim::SimEventStore;

#[tokio::test]
async fn older_snapshot_writer_cannot_regress_current_generation_or_coverage() {
    let store = SimEventStore::no_faults(277);
    let tenant = "default";
    let entity_type = "Doc";
    let persistence_id = format!("{tenant}:{entity_type}:snapshot-writer-race");
    let signature = "v4:path";
    store
        .save_snapshot(&persistence_id, 10, b"newer")
        .await
        .expect("commit newer snapshot writer");
    let revision = store
        .begin_key_index_backfill(tenant, entity_type, signature)
        .await
        .expect("begin coverage epoch");
    assert!(
        store
            .mark_key_index_backfilled_if_revision(tenant, entity_type, signature, revision)
            .await
            .expect("publish coverage")
    );

    store
        .save_snapshot(&persistence_id, 5, b"delayed-older")
        .await
        .expect("complete delayed older snapshot writer");

    assert_eq!(
        store
            .load_snapshot(&persistence_id)
            .await
            .expect("load current snapshot"),
        Some((10, b"newer".to_vec())),
        "a delayed writer must not replace a newer authoritative snapshot"
    );
    assert_eq!(
        store
            .key_index_reconciliation_revision(tenant, entity_type)
            .await
            .expect("read unchanged coverage epoch"),
        revision,
        "an ignored older snapshot must not invalidate coverage"
    );
    assert_eq!(
        store
            .key_index_backfilled_types(tenant)
            .await
            .expect("read preserved coverage watermark"),
        vec![(entity_type.to_string(), signature.to_string())]
    );
}

#[tokio::test]
async fn same_sequence_snapshot_rewrite_invalidates_published_key_coverage() {
    let store = SimEventStore::no_faults(273);
    let tenant = "default";
    let entity_type = "Doc";
    let persistence_id = format!("{tenant}:{entity_type}:snapshot-rewrite");
    let signature = "v4:path";
    store
        .append(
            &persistence_id,
            0,
            &[PersistenceEnvelope {
                sequence_nr: 0,
                event_type: "Create".to_string(),
                payload: serde_json::json!({}),
                metadata: EventMetadata {
                    event_id: uuid::Uuid::nil(),
                    causation_id: uuid::Uuid::nil(),
                    correlation_id: uuid::Uuid::nil(),
                    timestamp: chrono::DateTime::UNIX_EPOCH,
                    actor_id: persistence_id.clone(),
                },
            }],
        )
        .await
        .expect("seed journal high-water");
    store
        .save_snapshot(&persistence_id, 1, b"before")
        .await
        .expect("seed captured snapshot baseline");
    let revision = store
        .begin_key_index_backfill(tenant, entity_type, signature)
        .await
        .expect("begin coverage epoch");
    assert!(
        store
            .mark_key_index_backfilled_if_revision(tenant, entity_type, signature, revision)
            .await
            .expect("publish coverage")
    );

    store
        .save_snapshot(&persistence_id, 1, b"before")
        .await
        .expect("repeat identical snapshot write");
    assert_eq!(
        store
            .key_index_reconciliation_revision(tenant, entity_type)
            .await
            .expect("read unchanged coverage epoch"),
        revision,
        "identical snapshot bytes and sequence must not churn the coverage epoch"
    );
    assert_eq!(
        store
            .key_index_backfilled_types(tenant)
            .await
            .expect("read preserved coverage watermark"),
        vec![(entity_type.to_string(), signature.to_string())],
        "identical snapshot writes must preserve published coverage"
    );

    store
        .save_snapshot(&persistence_id, 1, b"after")
        .await
        .expect("rewrite snapshot bytes at the journal high-water");

    assert!(
        store
            .key_index_reconciliation_revision(tenant, entity_type)
            .await
            .expect("read current coverage epoch")
            > revision,
        "changed snapshot baseline bytes must invalidate the published coverage epoch"
    );
    assert!(
        store
            .key_index_backfilled_types(tenant)
            .await
            .expect("read coverage watermarks")
            .is_empty(),
        "stale ownership rows must not remain authoritative after a snapshot rewrite"
    );
}
