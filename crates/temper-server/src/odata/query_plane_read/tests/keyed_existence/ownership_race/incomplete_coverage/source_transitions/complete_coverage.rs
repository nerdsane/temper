use std::sync::Arc;

use super::*;

fn build_complete_state_with_catalog(
    system_name: &str,
    seed: u64,
) -> (ServerState, SimEventStore, Arc<SimQueryPlane>) {
    let store = SimEventStore::no_faults(seed);
    let query_plane = Arc::new(SimQueryPlane::default());
    let mut state = build_order_state(system_name);
    let mut storage = StorageStack::from_sim(store.clone(), None);
    storage.query_plane = Some(query_plane.clone());
    state.set_storage_stack(storage);
    (state, store, query_plane)
}

async fn seed_live_catalog(
    query_plane: &SimQueryPlane,
    tenant: &TenantId,
    entity_id: &str,
    workspace: &str,
    path: &str,
    sequence_nr: u64,
) {
    let fields = serde_json::json!({
        "Id": entity_id,
        "WorkspaceId": workspace,
        "Path": path,
    });
    QueryPlaneStore::upsert_projection(
        query_plane,
        tenant.as_str(),
        "Order",
        entity_id,
        "Draft",
        &fields,
        &serde_json::json!({
            "entity_type": "Order",
            "entity_id": entity_id,
            "status": "Draft",
            "fields": fields,
            "sequence_nr": sequence_nr,
        }),
        sequence_nr,
    )
    .await
    .expect("seed stale live catalog generation");
}

fn deleted_snapshot(entity_id: &str, workspace: &str, path: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "entity_type": "Order",
        "entity_id": entity_id,
        "status": "Deleted",
        "item_count": 0,
        "fields": {
            "Id": entity_id,
            "Status": "Deleted",
            "WorkspaceId": workspace,
            "Path": path,
        },
    }))
    .expect("serialize deleted snapshot")
}

#[tokio::test]
async fn complete_key_lookup_prefers_equal_sequence_journal_over_snapshot_source() {
    let (_guard, _clock, _ids) = install_deterministic_context(257);
    let tenant = TenantId::default();
    let workspace = "ws-complete-source-replacement";
    let snapshot_path = "/complete-snapshot-generation";
    let journal_path = "/complete-journal-generation";
    let entity_id = "ord-complete-equal-sequence-source";
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    let (state, store) = build_order_state_with_sim("complete-source-replacement");

    EventStore::save_snapshot(
        &store,
        &persistence_id,
        1,
        &snapshot(entity_id, workspace, snapshot_path),
    )
    .await
    .expect("seed snapshot-only complete generation");
    EventStore::append_with_index_rows(
        &store,
        &persistence_id,
        0,
        &[complete_field_update(
            &persistence_id,
            entity_id,
            workspace,
            journal_path,
            "replace-complete-snapshot-source",
        )],
        &[key_row(workspace, journal_path)],
        &[],
        IndexReconciliation {
            keys: true,
            key_set_signature: Some(ORDER_KEY_SET_SIGNATURE.to_string()),
            vectors: false,
            snapshot_source: Default::default(),
        },
    )
    .await
    .expect("commit equal-sequence complete journal replacement");
    EventStore::mark_key_index_backfilled(
        &store,
        tenant.as_str(),
        "Order",
        ORDER_KEY_SET_SIGNATURE,
    )
    .await
    .expect("mark complete key coverage");

    let current = expect_read(
        read_path(&state, &tenant, workspace, journal_path).await,
        "complete source-replacement current-key read",
    );
    assert_eq!(current.entities.len(), 1);
    assert_eq!(current.entities[0]["entity_id"], entity_id);
    assert_eq!(current.entities[0]["fields"]["Path"], journal_path);
    assert_eq!(current.entities[0]["sequence_nr"], 1);
}

#[tokio::test]
async fn complete_key_lookup_does_not_revive_a_mismatched_snapshot_from_catalog() {
    let (_guard, _clock, _ids) = install_deterministic_context(274);
    let tenant = TenantId::default();
    let workspace = "ws-complete-snapshot-mismatch";
    let stale_path = "/stale-catalog-owner";
    let current_path = "/current-snapshot-owner";
    let entity_id = "ord-complete-snapshot-mismatch";
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    let (state, store, query_plane) =
        build_complete_state_with_catalog("complete-snapshot-mismatch", 274);

    EventStore::save_snapshot(
        &store,
        &persistence_id,
        3,
        &snapshot(entity_id, workspace, stale_path),
    )
    .await
    .expect("seed stale snapshot ownership generation");
    state.populate_key_index_from_snapshots(&tenant).await;
    EventStore::save_snapshot(
        &store,
        &persistence_id,
        3,
        &snapshot(entity_id, workspace, current_path),
    )
    .await
    .expect("replace ownership with a same-sequence imported snapshot generation");
    seed_live_catalog(&query_plane, &tenant, entity_id, workspace, stale_path, 3).await;
    EventStore::mark_key_index_backfilled(
        &store,
        tenant.as_str(),
        "Order",
        ORDER_KEY_SET_SIGNATURE,
    )
    .await
    .expect("restore migrated complete-coverage marker");

    match read_path(&state, &tenant, workspace, stale_path).await {
        Err(QueryPlaneReadError::KeyOwnershipUnstable) => {}
        Ok(result) => panic!(
            "an already-present snapshot must outrank a matching stale catalog/key generation and fail the inconsistent owner closed; returned entities: {:?}",
            result.entities
        ),
        Err(error) => panic!(
            "an already-present snapshot must fail closed as KeyOwnershipUnstable, got {}",
            query_read_error_name(&error)
        ),
    }
}

#[tokio::test]
async fn complete_key_lookup_does_not_revive_a_deleted_snapshot_from_catalog() {
    let (_guard, _clock, _ids) = install_deterministic_context(275);
    let tenant = TenantId::default();
    let workspace = "ws-complete-deleted-snapshot";
    let path = "/deleted-snapshot-owner";
    let entity_id = "ord-complete-deleted-snapshot";
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    let (state, store, query_plane) =
        build_complete_state_with_catalog("complete-deleted-snapshot", 275);

    EventStore::save_snapshot(
        &store,
        &persistence_id,
        3,
        &snapshot(entity_id, workspace, path),
    )
    .await
    .expect("seed stale live snapshot ownership");
    state.populate_key_index_from_snapshots(&tenant).await;
    EventStore::save_snapshot(
        &store,
        &persistence_id,
        3,
        &deleted_snapshot(entity_id, workspace, path),
    )
    .await
    .expect("replace durable source with a terminal snapshot");
    seed_live_catalog(&query_plane, &tenant, entity_id, workspace, path, 3).await;
    EventStore::mark_key_index_backfilled(
        &store,
        tenant.as_str(),
        "Order",
        ORDER_KEY_SET_SIGNATURE,
    )
    .await
    .expect("restore migrated complete-coverage marker");

    assert!(
        matches!(
            read_path(&state, &tenant, workspace, path).await,
            Err(QueryPlaneReadError::KeyOwnershipUnstable)
        ),
        "a terminal snapshot must never be revived by a stale live catalog row and must fail the inconsistent owner closed"
    );
}

#[tokio::test]
async fn catalog_only_owner_must_match_the_requested_declared_key() {
    let (_guard, _clock, _ids) = install_deterministic_context(276);
    let tenant = TenantId::default();
    let workspace = "ws-catalog-only-key-mismatch";
    let indexed_path = "/indexed-owner";
    let catalog_path = "/different-catalog-body";
    let entity_id = "ord-catalog-only-key-mismatch";
    let (state, store, query_plane) =
        build_complete_state_with_catalog("catalog-only-key-mismatch", 276);

    let contract_revision = EventStore::begin_key_index_backfill(
        &store,
        tenant.as_str(),
        "Order",
        ORDER_KEY_SET_SIGNATURE,
    )
    .await
    .expect("begin catalog-only key contract");
    EventStore::backfill_entity_keys(
        &store,
        tenant.as_str(),
        "Order",
        entity_id,
        0,
        KeyIndexBackfillFence {
            key_set_signature: ORDER_KEY_SET_SIGNATURE,
            contract_revision,
            expected_journal_sequence: 0,
            expected_entity_live: false,
            expected_snapshot: None,
        },
        &[key_row(workspace, indexed_path)],
    )
    .await
    .expect("seed catalog-only ownership row");
    EventStore::mark_key_index_backfilled_if_revision(
        &store,
        tenant.as_str(),
        "Order",
        ORDER_KEY_SET_SIGNATURE,
        contract_revision,
    )
    .await
    .expect("complete catalog-only key coverage");
    seed_live_catalog(&query_plane, &tenant, entity_id, workspace, catalog_path, 0).await;

    assert!(
        matches!(
            read_path(&state, &tenant, workspace, indexed_path).await,
            Err(QueryPlaneReadError::KeyOwnershipUnstable)
        ),
        "a complete key row must not certify a catalog-only body whose declared-key fields hash differently"
    );
}

#[tokio::test]
async fn complete_key_read_ignores_unrelated_over_budget_projection_debt() {
    let (_guard, _clock, _ids) = install_deterministic_context(277);
    let tenant = TenantId::default();
    let workspace = "ws-keyed-through-dirty-debt";
    let path = "/authoritative-owner";
    let entity_id = "ord-keyed-through-dirty-debt";
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    let (state, store, query_plane) =
        build_complete_state_with_catalog("keyed-through-dirty-debt", 277);

    EventStore::append_with_index_rows(
        &store,
        &persistence_id,
        0,
        &[complete_field_update(
            &persistence_id,
            entity_id,
            workspace,
            path,
            "seed-keyed-through-dirty-debt",
        )],
        &[key_row(workspace, path)],
        &[],
        IndexReconciliation {
            keys: true,
            key_set_signature: Some(ORDER_KEY_SET_SIGNATURE.to_string()),
            vectors: false,
            snapshot_source: Default::default(),
        },
    )
    .await
    .expect("seed complete authoritative owner");
    EventStore::mark_key_index_backfilled(
        &store,
        tenant.as_str(),
        "Order",
        ORDER_KEY_SET_SIGNATURE,
    )
    .await
    .expect("publish complete key coverage");

    for index in 0..=100 {
        query_plane.mark_dirty("Order", &format!("unrelated-dirty-{index:03}"));
    }
    assert_eq!(query_plane.dirty_count(), 101);

    let result = expect_read(
        read_path(&state, &tenant, workspace, path).await,
        "complete keyed read with unrelated projection debt",
    );
    assert_eq!(result.entities.len(), 1);
    assert_eq!(result.entities[0]["entity_id"], entity_id);
    assert_eq!(result.entities[0]["fields"]["Path"], path);
    assert_eq!(
        query_plane.dirty_count(),
        101,
        "an authoritative key read must not drain unrelated catalog/EAV repair debt"
    );
}
