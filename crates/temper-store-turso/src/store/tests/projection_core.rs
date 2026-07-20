use super::*;

#[tokio::test]
async fn policy_denial_patterns_roundtrip_and_merge() {
    let store = make_store("policy-denials").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());

    store
        .upsert_policy_denial_pattern(
            &tenant,
            Some("planner"),
            "read",
            "Issue",
            "ISSUE-1",
            "2026-03-23T10:00:00Z",
        )
        .await
        .unwrap();
    store
        .upsert_policy_denial_pattern(
            &tenant,
            Some("planner"),
            "read",
            "Issue",
            "ISSUE-2",
            "2026-03-23T11:00:00Z",
        )
        .await
        .unwrap();

    let rows = store.load_policy_denial_patterns(&tenant).await.unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.agent_type.as_deref(), Some("planner"));
    assert_eq!(row.action, "read");
    assert_eq!(row.resource_type, "Issue");
    assert_eq!(row.count, 2);
    assert_eq!(row.first_seen, "2026-03-23T10:00:00Z");
    assert_eq!(row.last_seen, "2026-03-23T11:00:00Z");

    let ids: Vec<String> = serde_json::from_str(&row.distinct_resource_ids_json).unwrap();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&"ISSUE-1".to_string()));
    assert!(ids.contains(&"ISSUE-2".to_string()));
}

#[tokio::test]
async fn migrate_is_idempotent() {
    let store = make_store("migrate-idempotent").await;

    store.migrate().await.unwrap();
    store.migrate().await.unwrap();
}

/// Regression: append must be durable (readable from a fresh connection)
/// before the caller receives the new sequence number.
///
/// This is the persist-before-return ordering guarantee: the event log must
/// reflect the written event for any subsequent reader, even one that opens
/// a new connection to the same database file.
#[tokio::test]
async fn append_is_durable_before_return() {
    let url = sqlite_test_url("persist-before-return");
    let store1 = TursoEventStore::new(&url, None)
        .await
        .expect("create store1");

    let persistence_id = "tenant-x:Widget:w-1";
    let new_seq = store1
        .append(
            persistence_id,
            0,
            &[test_envelope("Created", serde_json::json!({"id": "w-1"}))],
        )
        .await
        .expect("append");

    assert_eq!(new_seq, 1, "should return sequence 1 after first append");

    // Open a new independent connection to the same DB — simulates a second
    // reader or a process restart. The event must already be visible.
    let store2 = TursoEventStore::new(&url, None)
        .await
        .expect("create store2");
    let events = store2
        .read_events(persistence_id, 0)
        .await
        .expect("read from second connection");

    assert_eq!(
        events.len(),
        1,
        "event must be durable and readable from a fresh connection immediately after append"
    );
    assert_eq!(events[0].sequence_nr, 1);
    assert_eq!(events[0].event_type, "Created");
}

#[tokio::test]
async fn query_projection_roundtrip_updates_catalog_and_field_index() {
    let store = make_store("query-projection-roundtrip").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let entity_type = "Order";
    let entity_id = "ord-projection";

    store
        .upsert_query_projection(
            &tenant,
            entity_type,
            entity_id,
            "Draft",
            &serde_json::json!({
                "Title": "Projection Test",
                "Owner": "alice",
                "Count": 3,
            }),
            7,
        )
        .await
        .expect("upsert query projection");

    let title_matches = store
        .query_field_index(
            &tenant,
            entity_type,
            "field_name = ?3 AND field_value = ?4",
            vec!["Title".to_string(), "Projection Test".to_string()],
        )
        .await
        .expect("query field index by title");
    assert_eq!(title_matches, vec![entity_id.to_string()]);

    let counts = store
        .projected_entity_counts_by_tenant()
        .await
        .expect("load projected entity counts");
    assert_eq!(counts, vec![(tenant.clone(), 1)]);

    store
        .remove_query_projection(&tenant, entity_type, entity_id)
        .await
        .expect("remove query projection");

    let remaining = store
        .query_field_index(
            &tenant,
            entity_type,
            "field_name = ?3 AND field_value = ?4",
            vec!["Title".to_string(), "Projection Test".to_string()],
        )
        .await
        .expect("query field index after delete");
    assert!(
        remaining.is_empty(),
        "field index rows should be removed with the query projection"
    );

    let counts = store
        .projected_entity_counts_by_tenant()
        .await
        .expect("load projected entity counts after delete");
    assert!(
        counts.is_empty(),
        "entity catalog should be empty after removing the projection"
    );
}

#[tokio::test]
async fn query_field_index_page_orders_and_limits_inside_turso() {
    let store = make_store("query-field-index-page").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let entity_type = "SessionEntry";

    for sequence in [1_u64, 10, 2] {
        let entity_id = format!("entry-{sequence}");
        let fields = serde_json::json!({
            "SessionId": "ss-bounded",
            "Sequence": sequence,
        });
        let state = serde_json::json!({
            "entity_type": entity_type,
            "entity_id": entity_id,
            "status": "Active",
            "fields": fields,
            "sequence_nr": sequence,
            "events": [],
        });
        store
            .upsert_query_projection_with_state(
                &tenant,
                entity_type,
                &entity_id,
                "Active",
                state.get("fields").unwrap(),
                &state,
                sequence,
            )
            .await
            .unwrap();
    }

    let (ids, count) = store
        .query_field_index_page(
            &tenant,
            entity_type,
            "entity_id IN (SELECT entity_id FROM entity_field_index \
             WHERE tenant = ?1 AND entity_type = ?2 \
             AND field_name = ?3 AND field_value = ?4)",
            vec!["SessionId".to_string(), "ss-bounded".to_string()],
            &[("Sequence".to_string(), true)],
            0,
            1,
            true,
        )
        .await
        .unwrap();

    assert_eq!(ids, vec!["entry-10".to_string()]);
    assert_eq!(count, Some(3));

    let (ids, count) = store
        .query_field_index_page(
            &tenant,
            entity_type,
            "entity_id IN (SELECT entity_id FROM entity_field_index \
             WHERE tenant = ?1 AND entity_type = ?2 \
             AND field_name = ?3 AND field_value = ?4)",
            vec!["SessionId".to_string(), "ss-bounded".to_string()],
            &[("Sequence".to_string(), true)],
            0,
            1,
            false,
        )
        .await
        .unwrap();

    assert_eq!(ids, vec!["entry-10".to_string()]);
    assert_eq!(count, None);

    let missing_sequence_id = "entry-missing-sequence";
    let missing_sequence_fields = serde_json::json!({
        "SessionId": "ss-bounded",
    });
    let missing_sequence_state = serde_json::json!({
        "entity_type": entity_type,
        "entity_id": missing_sequence_id,
        "status": "Active",
        "fields": missing_sequence_fields,
        "sequence_nr": 99,
        "events": [],
    });
    store
        .upsert_query_projection_with_state(
            &tenant,
            entity_type,
            missing_sequence_id,
            "Active",
            missing_sequence_state.get("fields").unwrap(),
            &missing_sequence_state,
            99,
        )
        .await
        .unwrap();

    let (ids, count) = store
        .query_field_index_page(
            &tenant,
            entity_type,
            "entity_id IN (SELECT entity_id FROM entity_field_index \
             WHERE tenant = ?1 AND entity_type = ?2 \
             AND field_name = ?3 AND field_value = ?4)",
            vec!["SessionId".to_string(), "ss-bounded".to_string()],
            &[("Sequence".to_string(), true)],
            0,
            1,
            true,
        )
        .await
        .unwrap();

    assert_eq!(ids, vec![missing_sequence_id.to_string()]);
    assert_eq!(count, Some(4));
}

#[tokio::test]
async fn query_projection_batch_updates_catalog_and_field_index() {
    let store = make_store("query-projection-batch").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());

    store
        .upsert_query_projections(
            &tenant,
            &[
                QueryProjectionUpsert {
                    entity_type: "Order".to_string(),
                    entity_id: "ord-batch-a".to_string(),
                    status: "Draft".to_string(),
                    fields: serde_json::json!({
                        "Title": "Batch A",
                        "Owner": "alice",
                    }),
                    state: serde_json::json!({
                        "entity_type": "Order",
                        "entity_id": "ord-batch-a",
                        "status": "Draft",
                        "fields": {
                            "Title": "Batch A",
                            "Owner": "alice",
                        },
                        "sequence_nr": 2,
                    }),
                    indexed_fields: serde_json::json!({
                        "Title": "Batch A",
                        "Owner": "alice",
                    }),
                    sequence_nr: 2,
                    known_new: false,
                },
                QueryProjectionUpsert {
                    entity_type: "Order".to_string(),
                    entity_id: "ord-batch-b".to_string(),
                    status: "Ready".to_string(),
                    fields: serde_json::json!({
                        "Title": "Batch B",
                        "Owner": "bob",
                    }),
                    state: serde_json::json!({
                        "entity_type": "Order",
                        "entity_id": "ord-batch-b",
                        "status": "Ready",
                        "fields": {
                            "Title": "Batch B",
                            "Owner": "bob",
                        },
                        "sequence_nr": 3,
                    }),
                    indexed_fields: serde_json::json!({
                        "Title": "Batch B",
                        "Owner": "bob",
                    }),
                    sequence_nr: 3,
                    known_new: false,
                },
            ],
        )
        .await
        .expect("batch projection upsert");

    let owner_matches = store
        .query_field_index(
            &tenant,
            "Order",
            "field_name = ?3 AND field_value = ?4",
            vec!["Owner".to_string(), "alice".to_string()],
        )
        .await
        .expect("query field index by owner");
    assert_eq!(owner_matches, vec!["ord-batch-a".to_string()]);

    let rows = store
        .load_entity_catalog_rows(
            &tenant,
            "Order",
            &["ord-batch-a".to_string(), "ord-batch-b".to_string()],
        )
        .await
        .expect("load catalog rows");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].sequence_nr, 2);
    assert_eq!(rows[1].sequence_nr, 3);
}

#[tokio::test]
async fn query_projection_batch_can_store_fields_without_indexing_them() {
    let store = make_store("query-projection-batch-index-subset").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());

    store
        .upsert_query_projections(
            &tenant,
            &[QueryProjectionUpsert {
                entity_type: "Blob".to_string(),
                entity_id: "blob-index-subset".to_string(),
                status: "Durable".to_string(),
                fields: serde_json::json!({
                    "Id": "blob-index-subset",
                    "RepositoryId": "repo-1",
                    "CanonicalBytes": "full-canonical-payload",
                }),
                state: serde_json::json!({
                    "entity_type": "Blob",
                    "entity_id": "blob-index-subset",
                    "status": "Durable",
                    "fields": {
                        "Id": "blob-index-subset",
                        "RepositoryId": "repo-1",
                        "CanonicalBytes": "full-canonical-payload",
                    },
                    "sequence_nr": 1,
                }),
                indexed_fields: serde_json::json!({
                    "Id": "blob-index-subset",
                    "RepositoryId": "repo-1",
                }),
                sequence_nr: 1,
                known_new: true,
            }],
        )
        .await
        .expect("batch projection upsert");

    let rows = store
        .load_entity_catalog_rows(&tenant, "Blob", &["blob-index-subset".to_string()])
        .await
        .expect("load catalog row");
    assert_eq!(
        rows[0].fields["CanonicalBytes"],
        serde_json::json!("full-canonical-payload")
    );

    let canonical_matches = store
        .query_field_index(
            &tenant,
            "Blob",
            "field_name = ?3 AND field_value = ?4",
            vec![
                "CanonicalBytes".to_string(),
                "full-canonical-payload".to_string(),
            ],
        )
        .await
        .expect("query canonical field");
    assert!(
        canonical_matches.is_empty(),
        "filtered fields should stay out of entity_field_index"
    );
}
