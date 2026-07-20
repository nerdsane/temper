use super::*;

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
