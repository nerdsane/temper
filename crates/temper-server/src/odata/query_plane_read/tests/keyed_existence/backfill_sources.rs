use super::*;
use temper_runtime::persistence::{EntityKeyRow, IndexReconciliation};
use temper_runtime::scheduler::{install_deterministic_context, sim_now};

#[tokio::test]
async fn key_backfill_prefers_journal_over_equal_sequence_snapshot_source() {
    let (_guard, _clock, _ids) = install_deterministic_context(265);
    let tenant = TenantId::default();
    let workspace = "ws-backfill-source";
    let snapshot_path = "/snapshot-generation";
    let journal_path = "/journal-generation";
    let entity_id = "ord-backfill-equal-sequence-source";
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    let (state, store) = build_order_state_with_sim("backfill-source-replacement");

    let snapshot = serde_json::json!({
        "entity_type": "Order",
        "entity_id": entity_id,
        "status": "Draft",
        "item_count": 0,
        "fields": {
            "Id": entity_id,
            "Status": "Draft",
            "WorkspaceId": workspace,
            "Path": snapshot_path,
        },
    });
    EventStore::save_snapshot(
        &store,
        &persistence_id,
        1,
        &serde_json::to_vec(&snapshot).expect("serialize stale snapshot generation"),
    )
    .await
    .expect("seed snapshot-only generation");

    let timestamp = sim_now();
    EventStore::append_with_index_rows(
        &store,
        &persistence_id,
        0,
        &[PersistenceEnvelope {
            sequence_nr: 0,
            event_type: "Temper.Internal.FieldUpdate.v1".to_string(),
            payload: serde_json::json!({
                "schema": "temper.field-update.v1",
                "fields": {
                    "Id": entity_id,
                    "WorkspaceId": workspace,
                    "Path": journal_path,
                },
                "replace": false,
                "idempotency_key": "backfill-source-replacement",
            }),
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp,
                actor_id: persistence_id.clone(),
            },
        }],
        &[EntityKeyRow {
            key_name: "ws_path".to_string(),
            key_hash: ws_path_hash(workspace, journal_path),
        }],
        &[],
        IndexReconciliation {
            keys: true,
            key_set_signature: Some(ORDER_KEY_SET_SIGNATURE.to_string()),
            vectors: false,
        },
    )
    .await
    .expect("replace snapshot-only source with an equal-sequence journal generation");

    state.populate_key_index_from_snapshots(&tenant).await;

    assert!(
        state
            .key_index_backfill_complete(&tenant, "Order", ORDER_KEY_SET_SIGNATURE)
            .await,
        "the stable journal generation must permit coverage publication"
    );
    assert_eq!(
        EventStore::lookup_by_key(
            &store,
            tenant.as_str(),
            "Order",
            "ws_path",
            &ws_path_hash(workspace, journal_path),
        )
        .await
        .expect("lookup journal-derived ownership")
        .as_deref(),
        Some(entity_id),
        "full backfill must preserve the current journal-derived key"
    );
    assert_eq!(
        EventStore::lookup_by_key(
            &store,
            tenant.as_str(),
            "Order",
            "ws_path",
            &ws_path_hash(workspace, snapshot_path),
        )
        .await
        .expect("lookup stale snapshot ownership"),
        None,
        "an equal-sequence stale snapshot must never replace journal ownership"
    );
}
