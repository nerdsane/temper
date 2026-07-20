use super::*;

#[tokio::test]
async fn deleted_catalog_absence_is_closed_against_first_journal_generation() {
    let (_guard, _clock, _ids) = install_deterministic_context(258);
    let tenant = TenantId::default();
    let workspace = "ws-deleted-catalog-source";
    let stale_path = "/deleted-catalog";
    let journal_path = "/live-journal";
    let entity_id = "ord-deleted-catalog-source";
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    let inner = SimEventStore::no_faults(258);
    let stale_snapshot = serde_json::json!({
        "entity_type": "Order",
        "entity_id": entity_id,
        "status": "Deleted",
        "item_count": 0,
        "fields": {
            "Id": entity_id,
            "Status": "Deleted",
            "WorkspaceId": workspace,
            "Path": stale_path,
        },
    });
    EventStore::save_snapshot(
        &inner,
        &persistence_id,
        1,
        &serde_json::to_vec(&stale_snapshot).expect("serialize deleted snapshot"),
    )
    .await
    .expect("seed deleted snapshot-only source");
    let query_plane = Arc::new(SimQueryPlane::default());
    QueryPlaneStore::upsert_projection(
        query_plane.as_ref(),
        tenant.as_str(),
        "Order",
        entity_id,
        "Deleted",
        &stale_snapshot["fields"],
        &stale_snapshot,
        1,
    )
    .await
    .expect("seed deleted catalog compatibility row");
    let boundary_calls = Arc::new(AtomicUsize::new(0));
    let racing = BoundaryMutationStore {
        inner,
        persistence_id,
        entity_id: entity_id.to_string(),
        workspace: workspace.to_string(),
        path: journal_path.to_string(),
        expected_sequence: 0,
        trigger_call: 1,
        mode: BoundaryMutationMode::ReturnCapturedBoundary,
        boundary_calls: boundary_calls.clone(),
    };
    let mut state = build_order_state("deleted-catalog-source-transition");
    install_boundary_store(&mut state, racing, Some(query_plane));

    let current = expect_read(
        read_path(&state, &tenant, workspace, journal_path).await,
        "deleted catalog source transition must replay the first journal generation",
    );
    assert_eq!(current.entities.len(), 1);
    assert_eq!(current.entities[0]["entity_id"], entity_id);
    assert_eq!(current.entities[0]["fields"]["Path"], journal_path);
    assert!(boundary_calls.load(Ordering::SeqCst) >= 2);
}
