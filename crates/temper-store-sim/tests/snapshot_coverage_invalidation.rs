//! Regression for snapshot baseline changes after key coverage publication.

use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_store_sim::SimEventStore;

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
