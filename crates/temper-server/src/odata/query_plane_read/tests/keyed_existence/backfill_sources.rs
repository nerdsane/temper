use super::*;
use temper_runtime::persistence::{EntityKeyRow, EventMetadata, IndexReconciliation};
use temper_runtime::scheduler::{install_deterministic_context, sim_now, sim_uuid};

mod snapshot_rewrite;

fn legacy_snapshot(entity_id: &str, workspace: &str, path: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "entity_type": "Order",
        "entity_id": entity_id,
        "status": "Draft",
        "item_count": 0,
        "fields": {
            "Id": entity_id,
            "Status": "Draft",
            "WorkspaceId": workspace,
            "Path": path,
        },
    }))
    .expect("serialize legacy snapshot")
}

fn journal_path_delta(persistence_id: &str, path: &str, token: &str) -> PersistenceEnvelope {
    let timestamp = sim_now();
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: "Temper.Internal.FieldUpdate.v1".to_string(),
        payload: serde_json::json!({
            "schema": "temper.field-update.v1",
            "fields": {"Path": path},
            "replace": false,
            "idempotency_key": token,
        }),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp,
            actor_id: persistence_id.to_string(),
        },
    }
}

async fn read_exact_path(
    state: &ServerState,
    tenant: &TenantId,
    workspace: &str,
    path: &str,
) -> Result<QueryPlaneReadResult, QueryPlaneReadError> {
    let options = QueryOptions {
        filter: Some(ws_path_filter(workspace, path)),
        ..QueryOptions::default()
    };
    let security_ctx = SecurityContext::system();
    read_entity_set_page(QueryPlaneReadRequest {
        state,
        tenant,
        security_ctx: &security_ctx,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &options,
        budget: QueryPlaneReadBudget {
            default_page_size: 10,
            max_entities: 10,
        },
    })
    .await
}

fn require_read(
    result: Result<QueryPlaneReadResult, QueryPlaneReadError>,
    message: &str,
) -> QueryPlaneReadResult {
    match result {
        Ok(result) => result,
        Err(_) => panic!("{message}"),
    }
}

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

#[tokio::test]
async fn complete_key_read_uses_the_same_legacy_snapshot_baseline_as_backfill() {
    let (_guard, _clock, _ids) = install_deterministic_context(267);
    let tenant = TenantId::default();
    let workspace = "ws-hybrid-source";
    let journal_path = "/journal-delta";
    let entity_id = "ord-hybrid-source";
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    let (state, store) = build_order_state_with_sim("backfill-read-source-parity");

    EventStore::save_snapshot(
        &store,
        &persistence_id,
        1,
        &legacy_snapshot(entity_id, workspace, "/snapshot-baseline"),
    )
    .await
    .expect("seed legacy fields that are absent from the journal delta");
    EventStore::append(
        &store,
        &persistence_id,
        0,
        &[journal_path_delta(
            &persistence_id,
            journal_path,
            "hybrid-source-delta",
        )],
    )
    .await
    .expect("seed journal generation with only the changed key component");

    state.populate_key_index_from_snapshots(&tenant).await;
    assert!(
        state
            .key_index_backfill_complete(&tenant, "Order", ORDER_KEY_SET_SIGNATURE)
            .await,
        "stable hybrid source must publish coverage"
    );

    let result = require_read(
        read_exact_path(&state, &tenant, workspace, journal_path).await,
        "a covered owner must remain materializable from the source used by backfill",
    );
    assert_eq!(result.entities.len(), 1);
    assert_eq!(result.entities[0]["entity_id"], entity_id);
    assert_eq!(result.entities[0]["fields"]["WorkspaceId"], workspace);
    assert_eq!(result.entities[0]["fields"]["Path"], journal_path);
}

#[tokio::test]
async fn complete_key_read_materializes_snapshot_only_owner_without_bootstrapping_journal() {
    let (_guard, _clock, _ids) = install_deterministic_context(268);
    let tenant = TenantId::default();
    let workspace = "ws-snapshot-owner";
    let path = "/snapshot-only-owner";
    let entity_id = "ord-snapshot-only-owner";
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    let (state, store) = build_order_state_with_sim("complete-snapshot-only-owner");

    EventStore::save_snapshot(
        &store,
        &persistence_id,
        5,
        &legacy_snapshot(entity_id, workspace, path),
    )
    .await
    .expect("seed ADR-0077 snapshot-only owner");
    state.populate_key_index_from_snapshots(&tenant).await;
    assert!(
        state
            .key_index_backfill_complete(&tenant, "Order", ORDER_KEY_SET_SIGNATURE)
            .await,
        "snapshot-only owner must be included in complete coverage"
    );

    let result = require_read(
        read_exact_path(&state, &tenant, workspace, path).await,
        "snapshot-only indexed owner must remain readable",
    );
    assert_eq!(result.entities.len(), 1);
    assert_eq!(result.entities[0]["entity_id"], entity_id);
    assert_eq!(
        EventStore::journal_boundary(&store, &persistence_id)
            .await
            .expect("journal remains readable")
            .latest_sequence,
        0,
        "materializing a snapshot-only owner must not bootstrap a journal"
    );
}
