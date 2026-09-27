//! Bulk setup for read-only query-plane tests using the production batch writer.

use temper_runtime::tenant::TenantId;
use temper_store_turso::{QueryProjectionUpsert, TursoEventStore};

/// Construct exactly the catalog state and indexed fields used by single-row setup.
pub(crate) fn fresh_order_projection(
    entity_id: String,
    mut fields: serde_json::Value,
    sequence_nr: u64,
) -> QueryProjectionUpsert {
    fields["Id"] = serde_json::json!(entity_id);
    let state = serde_json::json!({
        "entity_type": "Order",
        "entity_id": entity_id,
        "status": "Created",
        "fields": fields,
        "sequence_nr": sequence_nr,
        "events": [],
    });
    QueryProjectionUpsert {
        entity_type: "Order".to_string(),
        entity_id,
        status: "Created".to_string(),
        indexed_fields: fields.clone(),
        fields,
        state,
        sequence_nr,
        known_new: true,
    }
}

/// Populate an empty, isolated Order projection store without one commit per row.
pub(crate) async fn seed_fresh_order_projections(
    store: &TursoEventStore,
    tenant: &TenantId,
    projections: &[QueryProjectionUpsert],
) {
    assert!(
        store
            .query_field_index(tenant.as_str(), "Order", "1 = 1", Vec::new())
            .await
            .expect("check empty fixture index")
            .is_empty(),
        "known_new fixture rows require an empty isolated store"
    );
    for batch in projections.chunks(100) {
        store
            .upsert_query_projections(tenant.as_str(), batch)
            .await
            .expect("seed fresh query projections");
    }

    let mut indexed_ids = store
        .query_field_index(tenant.as_str(), "Order", "1 = 1", Vec::new())
        .await
        .expect("load fixture index");
    indexed_ids.sort();
    let mut expected = projections.iter().collect::<Vec<_>>();
    expected.sort_by(|left, right| left.entity_id.cmp(&right.entity_id));
    assert_eq!(
        indexed_ids,
        expected
            .iter()
            .map(|projection| projection.entity_id.clone())
            .collect::<Vec<_>>(),
        "all fixture rows must be present in the durable field index"
    );
    // Check the complete persisted input, not only the row selected by a test.
    for batch in expected.chunks(100) {
        let ids = batch
            .iter()
            .map(|projection| projection.entity_id.clone())
            .collect::<Vec<_>>();
        let persisted = store
            .load_entity_catalog_rows(tenant.as_str(), "Order", &ids)
            .await
            .expect("load seeded catalog rows");
        assert_eq!(persisted.len(), batch.len());
        for (row, projection) in persisted.iter().zip(batch) {
            assert_eq!(row.entity_id, projection.entity_id);
            assert_eq!(row.status, projection.status);
            assert_eq!(row.fields, projection.fields);
            assert_eq!(row.state.as_ref(), Some(&projection.state));
            assert_eq!(row.sequence_nr, projection.sequence_nr);
        }
    }
}
