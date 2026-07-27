//! Sparse selected catalog reads for OData list projections.

use std::collections::BTreeSet;

use temper_runtime::persistence::PersistenceError;

use crate::PostgresEventStore;
use crate::platform::{PostgresEntityCatalogRow, decoded_projection_sequence, storage_error};

impl PostgresEventStore {
    /// Batch-load catalog rows while projecting only the requested JSON fields.
    ///
    /// This keeps safe OData `$select` list reads from pulling the full
    /// `entity_catalog.state` and `entity_catalog.fields` JSONB payloads over
    /// the database boundary. The selected values are returned in the row's
    /// `fields` object and `state` is intentionally omitted.
    pub async fn load_selected_entity_catalog_rows_pg(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
        selected_fields: &[String],
    ) -> Result<Vec<PostgresEntityCatalogRow>, PersistenceError> {
        if entity_ids.is_empty() {
            return Ok(Vec::new());
        }

        let selected_fields = selected_fields
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let rows: Vec<(String, String, serde_json::Value, i64)> = crate::dbm::postgres_query_as!(
            "SELECT c.entity_id, \
                    c.status, \
                    COALESCE( \
                        ( \
                            SELECT jsonb_object_agg(k.field_name, selected.value) \
                            FROM unnest($4::text[]) AS k(field_name) \
                            CROSS JOIN LATERAL ( \
                                SELECT COALESCE( \
                                    c.state -> k.field_name, \
                                    c.state -> 'fields' -> k.field_name, \
                                    c.fields -> k.field_name \
                                ) AS value \
                            ) selected \
                            WHERE selected.value IS NOT NULL \
                        ), \
                        '{}'::jsonb \
                    ) AS fields, \
                    c.sequence_nr \
             FROM entity_catalog c \
             WHERE c.tenant = $1 AND c.entity_type = $2 AND c.entity_id = ANY($3) \
             ORDER BY c.entity_id",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_ids)
        .bind(&selected_fields)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;

        rows.into_iter()
            .map(|(entity_id, status, fields, sequence_nr)| {
                Ok(PostgresEntityCatalogRow {
                    sequence_nr: decoded_projection_sequence(sequence_nr, &entity_id)?,
                    entity_id,
                    status,
                    fields,
                    state: None,
                })
            })
            .collect()
    }
}
