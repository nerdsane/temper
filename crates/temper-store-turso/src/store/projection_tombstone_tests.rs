use super::{QueryProjectionUpsert, TursoEventStore};

fn sqlite_test_url(test_name: &str) -> String {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "temper-store-turso-{test_name}-{}.db",
        uuid::Uuid::new_v4()
    ));
    format!("file:{}", path.display())
}

#[tokio::test]
async fn durable_remove_high_water_blocks_delayed_single_and_batch_upserts() {
    let store = TursoEventStore::new(&sqlite_test_url("projection-tombstone"), None)
        .await
        .expect("create store");
    let tenant = "projection-delete-tenant";
    let fields = serde_json::json!({"title": "stale"});
    let state = serde_json::json!({
        "entity_type": "Ticket",
        "entity_id": "ticket-1",
        "status": "Open",
        "fields": fields,
        "sequence_nr": 1,
    });

    store
        .upsert_query_projection_with_state(
            tenant, "Ticket", "ticket-1", "Open", &fields, &state, 1,
        )
        .await
        .expect("seed projection");
    store
        .remove_query_projection_versioned(tenant, "Ticket", "ticket-1", 2)
        .await
        .expect("remove projection at sequence two");

    store
        .upsert_query_projection_with_state(
            tenant, "Ticket", "ticket-1", "Open", &fields, &state, 1,
        )
        .await
        .expect("delayed single upsert is ignored");
    store
        .upsert_query_projections(
            tenant,
            &[QueryProjectionUpsert {
                entity_type: "Ticket".to_string(),
                entity_id: "ticket-1".to_string(),
                status: "Open".to_string(),
                fields: fields.clone(),
                state: state.clone(),
                indexed_fields: fields.clone(),
                sequence_nr: 1,
                known_new: true,
            }],
        )
        .await
        .expect("delayed batch upsert is ignored");

    let rows = store
        .load_entity_catalog_rows(tenant, "Ticket", &["ticket-1".to_string()])
        .await
        .expect("load catalog rows");
    assert!(rows.is_empty(), "stale projection must remain absent");
    let ids = store
        .query_field_index(
            tenant,
            "Ticket",
            "field_name = ?3 AND field_value = ?4",
            vec!["title".to_string(), "stale".to_string()],
        )
        .await
        .expect("query field index");
    assert!(ids.is_empty(), "stale field rows must remain absent");

    let newer_state = serde_json::json!({
        "entity_type": "Ticket",
        "entity_id": "ticket-1",
        "status": "Reopened",
        "fields": {"title": "newer"},
        "sequence_nr": 3,
    });
    store
        .upsert_query_projection_with_state(
            tenant,
            "Ticket",
            "ticket-1",
            "Reopened",
            &newer_state["fields"],
            &newer_state,
            3,
        )
        .await
        .expect("newer projection is allowed");
    let rows = store
        .load_entity_catalog_rows(tenant, "Ticket", &["ticket-1".to_string()])
        .await
        .expect("load recreated catalog row");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].sequence_nr, 3);
    assert_eq!(rows[0].status, "Reopened");
}
