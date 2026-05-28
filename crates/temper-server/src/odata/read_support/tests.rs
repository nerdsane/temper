use super::{
    catalog_row_to_entity_body, catalog_row_to_selected_entity_body,
    catalog_select_projection_fields, select_entity_ids_for_materialization,
    should_read_catalog_for_materialization,
};
use crate::storage::EntityCatalogRow;
use temper_odata::query::types::{
    FilterExpr, ODataValue, OrderByClause, OrderDirection, QueryOptions,
};

#[test]
fn catalog_row_serializes_to_entity_state_shape() {
    let row = EntityCatalogRow {
        entity_id: "en-123".to_string(),
        status: "Published".to_string(),
        fields: serde_json::json!({"Name": "Foo", "Score": 7}),
        state: None,
        sequence_nr: 14,
    };
    let body = catalog_row_to_entity_body("DesignLanguage", "DesignLanguages", row);

    // Mirrors EntityState serialization keys so enrich_entity_response and
    // OData clients see no difference between catalog-served and actor-served bodies.
    assert_eq!(body["entity_type"], "DesignLanguage");
    assert_eq!(body["entity_id"], "en-123");
    assert_eq!(body["status"], "Published");
    assert_eq!(body["sequence_nr"], 14);
    assert_eq!(body["total_event_count"], 14);
    assert_eq!(body["item_count"], 0);
    assert_eq!(body["counters"], serde_json::json!({}));
    assert_eq!(body["booleans"], serde_json::json!({}));
    assert_eq!(body["lists"], serde_json::json!({}));
    assert_eq!(body["events"], serde_json::json!([]));
    assert_eq!(body["fields"]["Name"], "Foo");
    assert_eq!(body["fields"]["Score"], 7);
    assert_eq!(body["@odata.id"], "DesignLanguages('en-123')");
}

#[test]
fn catalog_row_body_matches_actor_serialization_for_single_key_path() {
    // The single-entity read path (handle_entity -> load_existing_entity_body)
    // expects the catalog-derived body to be a drop-in replacement for the
    // actor's serialized state. Verify the keyset matches what the actor
    // emits so enrich_entity_response and downstream clients see no diff.
    let row = EntityCatalogRow {
        entity_id: "fl-019dde81".to_string(),
        status: "Ready".to_string(),
        fields: serde_json::json!({
            "Id": "fl-019dde81",
            "Status": "Ready",
            "Name": "phosphor-command-grid.html",
            "MimeType": "text/html",
            "content_hash": "sha256:c74e77",
            "size_bytes": 8427,
        }),
        state: None,
        sequence_nr: 3,
    };
    let body = catalog_row_to_entity_body("File", "Files", row);
    let obj = body.as_object().expect("entity body is an object");
    // Required EntityState top-level keys.
    for required in [
        "entity_type",
        "entity_id",
        "status",
        "item_count",
        "counters",
        "booleans",
        "lists",
        "fields",
        "events",
        "total_event_count",
        "sequence_nr",
        "@odata.id",
    ] {
        assert!(
            obj.contains_key(required),
            "missing required key: {required}"
        );
    }
    // Verify @odata.id format matches what handle_entity uses.
    assert_eq!(body["@odata.id"], "Files('fl-019dde81')");
    // Fields blob is preserved verbatim.
    assert_eq!(body["fields"]["Name"], "phosphor-command-grid.html");
    assert_eq!(body["fields"]["content_hash"], "sha256:c74e77");
}

#[test]
fn catalog_row_prefers_full_state_payload_when_available() {
    let row = EntityCatalogRow {
        entity_id: "en-456".to_string(),
        status: "Published".to_string(),
        fields: serde_json::json!({"Name": "Indexed Only"}),
        state: Some(serde_json::json!({
            "entity_type": "DesignLanguage",
            "entity_id": "en-456",
            "status": "Published",
            "item_count": 3,
            "counters": {"Views": 42},
            "booleans": {"Featured": true},
            "lists": {"Tags": ["vapor", "terminal"]},
            "fields": {
                "Name": "Full State",
                "PrivateNotes": "not indexed"
            },
            "events": [{"action": "Publish"}],
            "total_event_count": 9,
            "sequence_nr": 9
        })),
        sequence_nr: 9,
    };

    let body = catalog_row_to_entity_body("DesignLanguage", "DesignLanguages", row);

    assert_eq!(body["item_count"], 3);
    assert_eq!(body["counters"]["Views"], 42);
    assert_eq!(body["booleans"]["Featured"], true);
    assert_eq!(body["lists"]["Tags"][0], "vapor");
    assert_eq!(body["fields"]["Name"], "Full State");
    assert_eq!(body["fields"]["PrivateNotes"], "not indexed");
    assert_eq!(body["events"], serde_json::json!([]));
    assert_eq!(body["@odata.id"], "DesignLanguages('en-456')");
}

#[test]
fn selected_catalog_body_resolves_fields_and_omits_unselected_payload() {
    let row = EntityCatalogRow {
        entity_id: "dl-selected".to_string(),
        status: "Published".to_string(),
        fields: serde_json::json!({
            "Id": "dl-selected",
            "Status": "Published",
            "Name": "Selected",
            "HugeCss": "x".repeat(32_000)
        }),
        state: Some(serde_json::json!({
            "entity_type": "DesignLanguage",
            "entity_id": "dl-selected",
            "status": "Published",
            "item_count": 2,
            "counters": {"Views": 9},
            "fields": {
                "Id": "dl-selected",
                "Status": "Published",
                "Name": "Selected",
                "HugeCss": "x".repeat(32_000)
            },
            "sequence_nr": 5
        })),
        sequence_nr: 5,
    };
    let select = vec![
        "Id".to_string(),
        "Name".to_string(),
        "Status".to_string(),
        "entity_id".to_string(),
        "sequence_nr".to_string(),
    ];

    let body =
        catalog_row_to_selected_entity_body("DesignLanguage", "DesignLanguages", row, &select);

    assert_eq!(body["Id"], "dl-selected");
    assert_eq!(body["Name"], "Selected");
    assert_eq!(body["Status"], "Published");
    assert_eq!(body["entity_id"], "dl-selected");
    assert_eq!(body["sequence_nr"], 5);
    assert_eq!(body["@odata.id"], "DesignLanguages('dl-selected')");
    assert!(body.get("HugeCss").is_none());
    assert!(body.get("fields").is_none());
    assert!(body.get("counters").is_none());
}

#[test]
fn selected_catalog_projection_only_applies_to_safe_select_only_reads() {
    let selected = QueryOptions {
        select: Some(vec!["Id".to_string(), "Status".to_string()]),
        ..QueryOptions::default()
    };
    assert_eq!(
        catalog_select_projection_fields(&selected).map(|fields| fields.len()),
        Some(2)
    );

    let filtered = QueryOptions {
        select: Some(vec!["Id".to_string()]),
        filter: Some(FilterExpr::Literal(ODataValue::Boolean(true))),
        ..QueryOptions::default()
    };
    assert!(catalog_select_projection_fields(&filtered).is_none());

    let ordered = QueryOptions {
        select: Some(vec!["Id".to_string()]),
        orderby: Some(vec![OrderByClause {
            property: "Name".to_string(),
            direction: OrderDirection::Asc,
        }]),
        ..QueryOptions::default()
    };
    assert!(catalog_select_projection_fields(&ordered).is_none());
}

#[test]
fn pushed_down_filters_prefer_catalog_materialization() {
    assert!(should_read_catalog_for_materialization(true));
}

#[test]
fn default_pagination_applies_when_top_missing_and_no_filter_orderby() {
    let ids: Vec<String> = (0..150).map(|i| format!("id-{i}")).collect();
    let opts = QueryOptions {
        count: Some(true),
        ..QueryOptions::default()
    };

    let selected = select_entity_ids_for_materialization(ids, &opts, 100, 1000, false).unwrap();

    assert_eq!(selected.entity_ids.len(), 100);
    assert_eq!(selected.entity_ids.first().unwrap(), "id-0");
    assert_eq!(selected.entity_ids.last().unwrap(), "id-99");
    assert_eq!(selected.precomputed_count, Some(150));
    assert_eq!(selected.apply_options.top, None);
    assert_eq!(selected.apply_options.skip, None);
    assert_eq!(selected.apply_options.count, None);
}

#[test]
fn explicit_skip_top_are_applied_before_materialization() {
    let ids: Vec<String> = (0..50).map(|i| format!("id-{i}")).collect();
    let opts = QueryOptions {
        top: Some(10),
        skip: Some(5),
        count: Some(true),
        ..QueryOptions::default()
    };

    let selected = select_entity_ids_for_materialization(ids, &opts, 100, 1000, false).unwrap();

    assert_eq!(selected.entity_ids.len(), 10);
    assert_eq!(selected.entity_ids.first().unwrap(), "id-5");
    assert_eq!(selected.entity_ids.last().unwrap(), "id-14");
    assert_eq!(selected.precomputed_count, Some(50));
}

#[test]
fn filtered_query_materialises_all_entities_under_safety_cap() {
    // With max_entities=1000, the safety cap is 10_000.
    // 2500 entities should ALL be materialised (no truncation).
    let ids: Vec<String> = (0..2500).map(|i| format!("id-{i}")).collect();
    let opts = QueryOptions {
        filter: Some(FilterExpr::Literal(ODataValue::Boolean(true))),
        orderby: Some(vec![OrderByClause {
            property: "Status".to_string(),
            direction: OrderDirection::Asc,
        }]),
        ..QueryOptions::default()
    };

    let selected = select_entity_ids_for_materialization(ids, &opts, 100, 1000, false).unwrap();

    assert_eq!(selected.entity_ids.len(), 2500);
    assert_eq!(selected.precomputed_count, None);
    assert_eq!(selected.apply_options.top, Some(100));
}

#[test]
fn safety_cap_rejects_incomplete_filtered_reads() {
    // With max_entities=1000, the safety cap is 10_000.
    // 15_000 entities must not be silently truncated.
    let ids: Vec<String> = (0..15_000).map(|i| format!("id-{i}")).collect();
    let opts = QueryOptions {
        filter: Some(FilterExpr::Literal(ODataValue::Boolean(true))),
        ..QueryOptions::default()
    };

    let error = select_entity_ids_for_materialization(ids, &opts, 100, 1000, false).unwrap_err();

    assert_eq!(error.candidate_count, 15_000);
    assert_eq!(error.candidate_budget, 10_000);
}

#[test]
fn safety_cap_rejects_incomplete_row_authorized_reads() {
    let ids: Vec<String> = (0..15_000).map(|i| format!("id-{i}")).collect();
    let opts = QueryOptions::default();

    let error = select_entity_ids_for_materialization(ids, &opts, 100, 1000, true).unwrap_err();

    assert_eq!(error.candidate_count, 15_000);
    assert_eq!(error.candidate_budget, 10_000);
}
