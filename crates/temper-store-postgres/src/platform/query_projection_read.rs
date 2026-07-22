//! PostgreSQL query-projection reads.

use super::*;

impl PostgresEventStore {
    pub async fn query_field_index(
        &self,
        tenant: &str,
        entity_type: &str,
        where_clause: &str,
        params: Vec<String>,
    ) -> Result<Vec<String>, PersistenceError> {
        let clause = postgres_placeholders(where_clause, params.len() + 2);
        let sql = format!(
            "SELECT entity_id FROM entity_catalog \
             WHERE tenant = $1 AND entity_type = $2 AND ({clause}) \
             ORDER BY entity_id"
        );
        let tagged_sql = crate::dbm::tag_sql(&sql);
        let mut query = sqlx::query_scalar::<_, String>(tagged_sql.as_ref())
            .bind(tenant)
            .bind(entity_type);
        for param in params {
            query = query.bind(param);
        }
        query.fetch_all(self.pool()).await.map_err(storage_error)
    }

    pub async fn load_query_projection_fields_many(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
        field_names: &[&str],
    ) -> Result<Vec<PostgresProjectedEntityFieldsRow>, PersistenceError> {
        if entity_ids.is_empty() {
            return Ok(Vec::new());
        }

        let requested_fields: BTreeSet<String> = field_names
            .iter()
            .map(|field| (*field).to_string())
            .collect();
        let field_names = requested_fields.iter().cloned().collect::<Vec<_>>();
        let rows = crate::dbm::postgres_query!(
            "SELECT c.entity_id, c.status, f.field_name, f.field_value \
             FROM entity_catalog c \
             LEFT JOIN entity_field_index f \
               ON c.tenant = f.tenant \
              AND c.entity_type = f.entity_type \
              AND c.entity_id = f.entity_id \
              AND f.field_name = ANY($4) \
             WHERE c.tenant = $1 \
               AND c.entity_type = $2 \
               AND c.entity_id = ANY($3) \
             ORDER BY c.entity_id, f.field_name",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_ids)
        .bind(&field_names)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;

        let mut by_entity = BTreeMap::<String, PostgresProjectedEntityFieldsRow>::new();
        for row in rows {
            let entity_id: String = row.get("entity_id");
            let status: String = row.get("status");
            let field_name: Option<String> = row.get("field_name");
            let field_value: Option<String> = row.get("field_value");
            let entry = by_entity.entry(entity_id.clone()).or_insert_with(|| {
                PostgresProjectedEntityFieldsRow {
                    entity_id: entity_id.clone(),
                    status,
                    fields: requested_fields
                        .iter()
                        .map(|field| (field.clone(), None))
                        .collect(),
                }
            });
            if let Some(field_name) = field_name
                && requested_fields.contains(&field_name)
            {
                entry.fields.insert(field_name, field_value);
            }
        }

        Ok(entity_ids
            .iter()
            .filter_map(|entity_id| by_entity.remove(entity_id))
            .collect())
    }

    pub async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Vec<(String, u64)>, PersistenceError> {
        let rows: Vec<(String, i64)> = crate::dbm::postgres_query_as!(
            "SELECT tenant, COUNT(*)::bigint FROM entity_catalog GROUP BY tenant ORDER BY tenant",
        )
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows
            .into_iter()
            .map(|(tenant, count)| (tenant, count as u64))
            .collect())
    }

    /// Return a bounded set of entities requiring exact-source projection repair.
    pub async fn dirty_query_projection_entity_ids(
        &self,
        tenant: &str,
        entity_type: &str,
        limit: usize,
    ) -> Result<Vec<String>, PersistenceError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        crate::dbm::postgres_query_scalar!(
            "SELECT entity_id FROM query_projection_dirty \
             WHERE tenant = $1 AND entity_type = $2 \
             ORDER BY entity_id LIMIT $3",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(limit)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)
    }

    /// Batch-load full entity catalog rows for a list of entity IDs.
    ///
    /// Returns only rows that exist in the catalog. IDs without a row in the
    /// projection are silently omitted from the result, leaving the caller
    /// free to fall back to the actor path on a per-id basis.
    pub async fn load_entity_catalog_rows_pg(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
    ) -> Result<Vec<crate::platform::PostgresEntityCatalogRow>, PersistenceError> {
        if entity_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<(
            String,
            String,
            serde_json::Value,
            Option<serde_json::Value>,
            i64,
        )> = crate::dbm::postgres_query_as!(
            "SELECT entity_id, status, fields, state, sequence_nr \
             FROM entity_catalog \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = ANY($3) \
             ORDER BY entity_id",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_ids)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows
            .into_iter()
            .map(|(entity_id, status, fields, state, seq)| {
                crate::platform::PostgresEntityCatalogRow {
                    entity_id,
                    status,
                    fields,
                    state,
                    sequence_nr: seq.max(0) as u64,
                }
            })
            .collect())
    }
}
