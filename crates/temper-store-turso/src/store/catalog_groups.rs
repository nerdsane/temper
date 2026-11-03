//! The catalog rows of one entity type in some states, grouped by status and
//! by the values of a few fields, in one read.

use temper_runtime::persistence::{CatalogGroupRow, PersistenceError, storage_error};
use tracing::instrument;

use super::TursoEventStore;
use super::query_page::turso_json_path_literal;

impl TursoEventStore {
    /// The catalog rows of `entity_type` in one of `statuses` that no journal
    /// event has passed and whose fields are not wrapped under `fields` (the
    /// rows `projection_catalog_freshness` reads as current and unwrapped),
    /// grouped by status and by the JSON value of each of `fields`, with the
    /// ids in each group. The database does the grouping.
    #[instrument(skip_all, fields(
        otel.name = "turso.catalog_groups",
        tenant, entity_type, fields = fields.len(), statuses = statuses.len(),
    ))]
    pub async fn catalog_groups(
        &self,
        tenant: &str,
        entity_type: &str,
        fields: &[String],
        statuses: &[String],
    ) -> Result<Vec<CatalogGroupRow>, PersistenceError> {
        if statuses.is_empty() {
            return Ok(Vec::new());
        }
        let values: String = fields
            .iter()
            .map(|field| format!(", c.fields -> {}", turso_json_path_literal(field)))
            .collect();
        let placeholders = (0..statuses.len())
            .map(|index| format!("?{}", index + 3))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT c.status{values}, json_group_array(c.entity_id) \
             FROM entity_catalog c \
             LEFT JOIN entity_catalog_counters k \
               ON k.tenant = c.tenant AND k.entity_type = c.entity_type AND k.entity_id = c.entity_id \
             WHERE c.tenant = ?1 \
               AND c.entity_type = ?2 \
               AND c.status IN ({placeholders}) \
               AND json_type(c.fields, '$.fields') IS NOT 'object' \
               AND NOT EXISTS ( \
                 SELECT 1 FROM events e \
                 WHERE e.tenant = c.tenant \
                   AND e.entity_type = c.entity_type \
                   AND e.entity_id = c.entity_id \
                   AND e.sequence_nr > COALESCE(k.sequence_nr, c.sequence_nr) \
               ) \
             GROUP BY c.status{values}"
        );
        let mut params: Vec<crate::driver::Value> = vec![
            crate::driver::Value::from(tenant.to_string()),
            crate::driver::Value::from(entity_type.to_string()),
        ];
        params.extend(statuses.iter().cloned().map(crate::driver::Value::from));
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(&sql, crate::driver::params_from_iter(params))
            .await
            .map_err(storage_error)?;
        let malformed =
            |error: serde_json::Error| PersistenceError::Serialization(error.to_string());
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let status: String = row.get(0).map_err(storage_error)?;
            let mut values = Vec::with_capacity(fields.len());
            for index in 0..fields.len() {
                let raw: Option<String> = row.get(index + 1).map_err(storage_error)?;
                values.push(
                    raw.map(|text| serde_json::from_str(&text))
                        .transpose()
                        .map_err(malformed)?,
                );
            }
            let ids: String = row.get(fields.len() + 1).map_err(storage_error)?;
            let mut ids: Vec<String> = serde_json::from_str(&ids).map_err(malformed)?;
            ids.sort();
            out.push(CatalogGroupRow {
                status,
                values,
                ids,
            });
        }
        Ok(out)
    }
}
