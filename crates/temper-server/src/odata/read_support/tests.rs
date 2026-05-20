use super::{
    catalog_row_to_entity_body, select_entity_ids_for_materialization,
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

    let (selected, apply_opts, count) =
        select_entity_ids_for_materialization(ids, &opts, 100, 1000);

    assert_eq!(selected.len(), 100);
    assert_eq!(selected.first().unwrap(), "id-0");
    assert_eq!(selected.last().unwrap(), "id-99");
    assert_eq!(count, Some(150));
    assert_eq!(apply_opts.top, None);
    assert_eq!(apply_opts.skip, None);
    assert_eq!(apply_opts.count, None);
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

    let (selected, _apply_opts, count) =
        select_entity_ids_for_materialization(ids, &opts, 100, 1000);

    assert_eq!(selected.len(), 10);
    assert_eq!(selected.first().unwrap(), "id-5");
    assert_eq!(selected.last().unwrap(), "id-14");
    assert_eq!(count, Some(50));
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

    let (selected, apply_opts, count) =
        select_entity_ids_for_materialization(ids, &opts, 100, 1000);

    assert_eq!(selected.len(), 2500);
    assert_eq!(count, None);
    assert_eq!(apply_opts.top, Some(100));
}

#[test]
fn safety_cap_truncates_at_10x_max_entities() {
    // With max_entities=1000, the safety cap is 10_000.
    // 15_000 entities should be truncated to 10_000.
    let ids: Vec<String> = (0..15_000).map(|i| format!("id-{i}")).collect();
    let opts = QueryOptions {
        filter: Some(FilterExpr::Literal(ODataValue::Boolean(true))),
        ..QueryOptions::default()
    };

    let (selected, apply_opts, count) =
        select_entity_ids_for_materialization(ids, &opts, 100, 1000);

    assert_eq!(selected.len(), 10_000);
    assert_eq!(count, None);
    assert_eq!(apply_opts.top, Some(100));
}
