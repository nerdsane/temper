//! The catalog rows of one entity type in some states, grouped by status and
//! by the values of a few fields, in one read.

use sqlx::Row;
use temper_runtime::persistence::{CatalogGroupRow, PersistenceError};

use crate::PostgresEventStore;
use crate::platform::storage_error;

impl PostgresEventStore {
    /// The catalog rows of `entity_type` in one of `statuses` that no journal
    /// event has passed and whose fields are not wrapped under `fields` (the
    /// rows `projection_catalog_freshness_pg` reads as current and unwrapped),
    /// grouped by status and by the JSON value of each of `fields`, with the
    /// ids in each group. The database does the grouping.
    pub async fn catalog_groups_pg(
        &self,
        tenant: &str,
        entity_type: &str,
        fields: &[String],
        statuses: &[String],
    ) -> Result<Vec<CatalogGroupRow>, PersistenceError> {
        if statuses.is_empty() {
            return Ok(Vec::new());
        }
        let schema = self.schema.prefix();
        let values: String = (0..fields.len())
            .map(|index| format!(", (c.fields -> ${})::text", index + 4))
            .collect();
        let groups: String = (0..=fields.len())
            .map(|index| (index + 1).to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT c.status{values}, array_agg(c.entity_id ORDER BY c.entity_id) \
             FROM {schema}entity_catalog c \
             WHERE c.tenant = $1 \
               AND c.entity_type = $2 \
               AND c.status = ANY($3) \
               AND jsonb_typeof(c.fields -> 'fields') IS DISTINCT FROM 'object' \
               AND NOT EXISTS ( \
                 SELECT 1 FROM {schema}events e \
                 WHERE e.tenant = c.tenant \
                   AND e.entity_type = c.entity_type \
                   AND e.entity_id = c.entity_id \
                   AND e.sequence_nr > c.sequence_nr \
               ) \
             GROUP BY {groups}"
        );
        let tagged_sql = crate::dbm::tag_sql(&sql);
        let mut query = sqlx::query(tagged_sql.as_ref())
            .bind(tenant)
            .bind(entity_type)
            .bind(statuses);
        for field in fields {
            query = query.bind(field);
        }
        let rows = query.fetch_all(self.pool()).await.map_err(storage_error)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let status: String = row.try_get(0).map_err(storage_error)?;
            let mut values = Vec::with_capacity(fields.len());
            for index in 0..fields.len() {
                let raw: Option<String> = row.try_get(index + 1).map_err(storage_error)?;
                values.push(
                    raw.map(|text| serde_json::from_str(&text))
                        .transpose()
                        .map_err(|error| PersistenceError::Serialization(error.to_string()))?,
                );
            }
            let ids: Vec<String> = row.try_get(fields.len() + 1).map_err(storage_error)?;
            out.push(CatalogGroupRow {
                status,
                values,
                ids,
            });
        }
        Ok(out)
    }
}
