use super::*;

#[tokio::test]
async fn unchanged_projection_updates_catalog_without_rebuilding_field_rows() {
    let store = make_store("query-projection-stable-hash").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let entity_type = "Order";
    let entity_id = "ord-stable-projection";

    store
        .upsert_query_projection(
            &tenant,
            entity_type,
            entity_id,
            "Draft",
            &serde_json::json!({
                "Title": "Projection Test",
                "Owner": "alice",
            }),
            7,
        )
        .await
        .expect("initial projection upsert");

    let conn = store.connection().expect("connection");
    let mut rows = conn
        .query(
            "SELECT rowid FROM entity_field_index \
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND field_name = 'Title'",
            params![tenant.clone(), entity_type, entity_id],
        )
        .await
        .expect("query initial title row");
    let initial_row = rows
        .next()
        .await
        .expect("read initial title row")
        .expect("title row should exist");
    let initial_rowid = initial_row.get::<i64>(0).expect("initial rowid");

    store
        .upsert_query_projection(
            &tenant,
            entity_type,
            entity_id,
            "Draft",
            &serde_json::json!({
                "Title": "Projection Test",
                "Owner": "alice",
            }),
            8,
        )
        .await
        .expect("second projection upsert");

    let conn = store.connection().expect("connection");
    let mut rows = conn
        .query(
            "SELECT rowid FROM entity_field_index \
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND field_name = 'Title'",
            params![tenant.clone(), entity_type, entity_id],
        )
        .await
        .expect("query updated title row");
    let updated_row = rows
        .next()
        .await
        .expect("read updated title row")
        .expect("title row should still exist");
    let updated_rowid = updated_row.get::<i64>(0).expect("updated rowid");

    let mut catalog_rows = conn
        .query(
            "SELECT sequence_nr FROM entity_catalog \
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
            params![tenant, entity_type, entity_id],
        )
        .await
        .expect("query catalog row");
    let catalog_row = catalog_rows
        .next()
        .await
        .expect("read catalog row")
        .expect("catalog row should exist");
    let sequence_nr = catalog_row.get::<i64>(0).expect("catalog sequence_nr");

    assert_eq!(
        updated_rowid, initial_rowid,
        "unchanged projections should keep existing field index rows"
    );
    assert_eq!(
        sequence_nr, 8,
        "entity catalog should still advance to the latest sequence number"
    );
}

#[tokio::test]
async fn stale_projection_upsert_does_not_overwrite_newer_catalog_row() {
    let store = make_store("query-projection-stale-sequence-skip").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let entity_type = "App";
    let entity_id = "app-stale-projection";

    store
        .upsert_query_projection(
            &tenant,
            entity_type,
            entity_id,
            "Active",
            &serde_json::json!({
                "OwnerId": "owner-a",
                "Name": "registered",
                "LatestVersionHash": "newer",
            }),
            4,
        )
        .await
        .expect("fresh projection upsert");

    store
        .upsert_query_projection(
            &tenant,
            entity_type,
            entity_id,
            "Active",
            &serde_json::json!({
                "Name": "registered",
                "RepositoryId": "repo-a",
            }),
            2,
        )
        .await
        .expect("stale projection upsert is ignored");

    let conn = store.connection().expect("connection");
    let mut rows = conn
        .query(
            "SELECT sequence_nr, fields FROM entity_catalog \
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
            params![tenant, entity_type, entity_id],
        )
        .await
        .expect("query catalog row");
    let row = rows
        .next()
        .await
        .expect("read catalog row")
        .expect("catalog row should exist");
    let sequence_nr = row.get::<i64>(0).expect("catalog sequence_nr");
    let fields_json = row.get::<String>(1).expect("catalog fields");
    let fields: serde_json::Value =
        serde_json::from_str(&fields_json).expect("catalog fields are json");

    assert_eq!(sequence_nr, 4);
    assert_eq!(fields["OwnerId"], "owner-a");
    assert_eq!(fields["LatestVersionHash"], "newer");
    assert!(fields.get("RepositoryId").is_none());
}

#[tokio::test]
async fn load_query_projection_fields_many_returns_requested_fields_by_entity() {
    let store = make_store("query-projection-fields-many").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());

    store
        .upsert_query_projection(
            &tenant,
            "File",
            "file-a",
            "Ready",
            &serde_json::json!({
                "content_hash": "sha256:file-a",
                "mime_type": "application/json",
                "has_content": true,
                "size_bytes": 12,
            }),
            1,
        )
        .await
        .expect("upsert file-a projection");
    store
        .upsert_query_projection(
            &tenant,
            "File",
            "file-b",
            "Created",
            &serde_json::json!({
                "content_hash": "",
                "mime_type": "text/plain",
                "has_content": false,
            }),
            1,
        )
        .await
        .expect("upsert file-b projection");

    let rows = store
        .load_query_projection_fields_many(
            &tenant,
            "File",
            &[
                "file-a".to_string(),
                "file-b".to_string(),
                "missing".to_string(),
            ],
            &["content_hash", "mime_type", "has_content"],
        )
        .await
        .expect("load projected fields");

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].entity_id, "file-a");
    assert_eq!(rows[0].status, "Ready");
    assert_eq!(
        rows[0]
            .fields
            .get("content_hash")
            .and_then(|v| v.as_deref()),
        Some("sha256:file-a")
    );
    assert_eq!(
        rows[0].fields.get("mime_type").and_then(|v| v.as_deref()),
        Some("application/json")
    );
    assert_eq!(
        rows[0].fields.get("has_content").and_then(|v| v.as_deref()),
        Some("true")
    );

    assert_eq!(rows[1].entity_id, "file-b");
    assert_eq!(rows[1].status, "Created");
    assert_eq!(
        rows[1].fields.get("has_content").and_then(|v| v.as_deref()),
        Some("false")
    );
    assert!(
        rows.iter().all(|row| row.entity_id != "missing"),
        "missing entity ids should be omitted"
    );
}

#[tokio::test]
async fn load_entity_catalog_rows_returns_full_projected_fields() {
    let store = make_store("entity-catalog-rows-full-fields").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());

    let fields = serde_json::json!({
        "Path": "/notes/readme.md",
        "WorkspaceId": "ws-a",
        "MimeType": "text/markdown",
        "HasContent": true,
        "content_hash": "sha256:file-a",
        "has_content": true,
        "size_bytes": 12,
        "nested": { "kept": true },
    });
    let state = serde_json::json!({
        "entity_type": "File",
        "entity_id": "file-a",
        "status": "Ready",
        "item_count": 2,
        "counters": {"Views": 3},
        "booleans": {"Pinned": true},
        "lists": {"Tags": ["docs"]},
        "fields": fields.clone(),
        "events": [],
        "total_event_count": 7,
        "sequence_nr": 7,
    });
    store
        .upsert_query_projection_with_state(&tenant, "File", "file-a", "Ready", &fields, &state, 7)
        .await
        .expect("upsert file projection");

    let rows = store
        .load_entity_catalog_rows(
            &tenant,
            "File",
            &["file-a".to_string(), "missing".to_string()],
        )
        .await
        .expect("load catalog rows");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].entity_id, "file-a");
    assert_eq!(rows[0].status, "Ready");
    assert_eq!(rows[0].sequence_nr, 7);
    assert_eq!(rows[0].fields["Path"], "/notes/readme.md");
    assert_eq!(rows[0].fields["WorkspaceId"], "ws-a");
    assert_eq!(rows[0].fields["MimeType"], "text/markdown");
    assert_eq!(rows[0].fields["HasContent"], true);
    assert_eq!(rows[0].fields["content_hash"], "sha256:file-a");
    assert_eq!(rows[0].fields["has_content"], true);
    assert_eq!(rows[0].fields["size_bytes"], 12);
    assert_eq!(rows[0].fields["nested"]["kept"], true);
    assert_eq!(rows[0].state.as_ref().unwrap()["counters"]["Views"], 3);
    assert_eq!(rows[0].state.as_ref().unwrap()["booleans"]["Pinned"], true);
    assert_eq!(rows[0].state.as_ref().unwrap()["lists"]["Tags"][0], "docs");
}

#[tokio::test]
async fn query_projection_status_follows_projected_state_over_fallback_argument() {
    let store = make_store("query-projection-status-state-parity").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let fields = serde_json::json!({
        "Title": "Default lifecycle row",
        "Status": "Draft",
    });
    let state = serde_json::json!({
        "entity_type": "Order",
        "entity_id": "ord-draft",
        "status": "Draft",
        "item_count": 0,
        "counters": {},
        "booleans": {},
        "lists": {},
        "fields": fields.clone(),
        "events": [],
        "total_event_count": 1,
        "sequence_nr": 1,
    });

    store
        .upsert_query_projection_with_state(
            &tenant,
            "Order",
            "ord-draft",
            "Published",
            &fields,
            &state,
            1,
        )
        .await
        .expect("upsert projection with stale fallback status");

    let rows = store
        .load_entity_catalog_rows(&tenant, "Order", &["ord-draft".to_string()])
        .await
        .expect("load catalog row");
    assert_eq!(rows[0].status, "Draft");

    let ids = store
        .query_field_index(&tenant, "Order", "status = ?3", vec!["Draft".to_string()])
        .await
        .expect("query catalog status");
    assert_eq!(ids, vec!["ord-draft".to_string()]);
}
