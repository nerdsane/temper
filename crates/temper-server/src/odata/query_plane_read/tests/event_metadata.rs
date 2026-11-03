//! Residual metadata filters must be counted across every candidate page.
use super::*;
use temper_odata::query::types::parse_query_options;

#[tokio::test]
async fn compound_event_metadata_filters_count_only_matching_rows() {
    let dir = tempfile::tempdir().unwrap();
    let store = TursoEventStore::new(
        &format!("file:{}", dir.path().join("metadata.db").display()),
        None,
    )
    .await
    .unwrap();
    let mut state = build_order_state("metadata-count");
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    let tenant = TenantId::default();
    for (id, via, first) in [
        ("a", "timer", "2026-01-01T00:00:00.000000000Z"),
        ("b", "request", "2026-01-02T00:00:00.000000000Z"),
        ("c", "request", "2026-01-03T00:00:00.000000000Z"),
    ] {
        let by = match id {
            "b" => Some("alice"),
            "c" => Some("bob"),
            _ => None,
        };
        store
            .upsert_query_projection_with_state(
                tenant.as_str(),
                "Order",
                id,
                "Created",
                &serde_json::json!({"Id": id, "Notes": "queue"}),
                &serde_json::json!({
                    "status": "Created", "last_event_via": via, "first_event_at": first,
                    "last_event_at": first, "last_event_by": by,
                }),
                1,
            )
            .await
            .unwrap();
    }
    // Metadata is stored outside entity_field_index. It must remain a residual
    // filter even when an indexed application field narrows the candidates.
    for (filter, expected_id) in [
        ("Status eq 'Created' and last_event_via eq 'timer'", "a"),
        (
            "Status eq 'Created' and first_event_at eq '2026-01-01T01:00:00%2B01:00'",
            "a",
        ),
        (
            "Status eq 'Created' and last_event_at eq '2026-01-02T01:00:00%2B01:00'",
            "b",
        ),
        ("Status eq 'Created' and last_event_by eq 'alice'", "b"),
        ("Notes eq 'queue' and last_event_via eq 'timer'", "a"),
        (
            "Notes eq 'queue' and first_event_at eq '2026-01-01T01:00:00%2B01:00'",
            "a",
        ),
        (
            "Notes eq 'queue' and last_event_at eq '2026-01-02T01:00:00%2B01:00'",
            "b",
        ),
        ("Notes eq 'queue' and last_event_by eq 'alice'", "b"),
    ] {
        let query_options =
            parse_query_options(&format!("$filter={filter}&$top=1&$count=true")).unwrap();
        let result = read_entity_set_page(QueryPlaneReadRequest {
            state: &state,
            tenant: &tenant,
            security_ctx: &SecurityContext::system(),
            entity_type: "Order",
            entity_set_name: "Orders",
            query_options: &query_options,
            budget: QueryPlaneReadBudget {
                default_page_size: 1,
                max_entities: 100,
            },
        })
        .await
        .unwrap_or_else(|_| panic!("read failed: {filter}"));
        assert_eq!(result.count, Some(1), "{filter}");
        assert_eq!(result.entities.len(), 1);
        assert_eq!(result.entities[0]["entity_id"], expected_id, "{filter}");
    }
}
