//! Tenant-scoped feature-request and evolution-record persistence.

use super::*;

impl PostgresEventStore {
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_feature_request(
        &self,
        tenant: &str,
        id: &str,
        category: &str,
        description: &str,
        frequency: i64,
        trajectory_refs_json: &str,
        disposition: &str,
        developer_notes: Option<&str>,
    ) -> Result<(), PersistenceError> {
        let trajectory_refs = parse_json(trajectory_refs_json)?;
        crate::dbm::postgres_query!(
            "INSERT INTO feature_requests \
             (id, tenant, category, description, frequency, trajectory_refs, disposition, developer_notes, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now()) \
             ON CONFLICT (id) DO UPDATE SET \
                 category = EXCLUDED.category, description = EXCLUDED.description, frequency = EXCLUDED.frequency, \
                 trajectory_refs = EXCLUDED.trajectory_refs, disposition = EXCLUDED.disposition, \
                 developer_notes = EXCLUDED.developer_notes, updated_at = now() \
             WHERE feature_requests.tenant = EXCLUDED.tenant",
        )
        .bind(id)
        .bind(tenant)
        .bind(category)
        .bind(description)
        .bind(frequency)
        .bind(trajectory_refs)
        .bind(disposition)
        .bind(developer_notes)
        .execute(self.pool())
        .await
        .map_err(storage_error)
        .and_then(|result| {
            if result.rows_affected() == 0 {
                Err(PersistenceError::Storage(format!(
                    "feature request '{id}' is owned by another tenant"
                )))
            } else {
                Ok(result)
            }
        })?;
        Ok(())
    }

    pub async fn list_feature_requests(
        &self,
        tenant: &str,
        disposition: Option<&str>,
    ) -> Result<Vec<PostgresFeatureRequestRow>, PersistenceError> {
        let rows = crate::dbm::postgres_query!(
            "SELECT id, tenant, category, description, frequency, trajectory_refs, disposition, developer_notes, created_at, updated_at \
             FROM feature_requests \
             WHERE tenant = $1 AND ($2::text IS NULL OR disposition = $2) \
             ORDER BY frequency DESC, created_at DESC",
        )
        .bind(tenant)
        .bind(disposition)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows.into_iter().map(row_to_feature_request).collect())
    }

    pub async fn update_feature_request(
        &self,
        tenant: &str,
        id: &str,
        disposition: &str,
        developer_notes: Option<&str>,
    ) -> Result<bool, PersistenceError> {
        let result = crate::dbm::postgres_query!(
            "UPDATE feature_requests SET disposition = $3, developer_notes = $4, updated_at = now() \
             WHERE tenant = $1 AND id = $2",
        )
        .bind(tenant)
        .bind(id)
        .bind(disposition)
        .bind(developer_notes)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn insert_evolution_record(
        &self,
        record: PostgresEvolutionRecordInsert<'_>,
    ) -> Result<(), PersistenceError> {
        let PostgresEvolutionRecordInsert {
            tenant,
            id,
            record_type,
            status,
            created_by,
            derived_from,
            data_json,
        } = record;
        let payload = parse_json(data_json)?;
        crate::dbm::postgres_query!(
            "INSERT INTO evolution_records (id, tenant, record_type, status, created_by, derived_from, payload, timestamp) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, now())",
        )
        .bind(id)
        .bind(tenant)
        .bind(record_type)
        .bind(status)
        .bind(created_by)
        .bind(derived_from)
        .bind(payload)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    pub async fn get_evolution_record(
        &self,
        tenant: &str,
        id: &str,
    ) -> Result<Option<PostgresEvolutionRecordRow>, PersistenceError> {
        let row = crate::dbm::postgres_query!(
            "SELECT id, tenant, record_type, status, created_by, derived_from, payload, timestamp \
             FROM evolution_records WHERE tenant = $1 AND id = $2",
        )
        .bind(tenant)
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(row.map(row_to_evolution_record))
    }

    pub async fn list_evolution_records(
        &self,
        tenant: &str,
        record_type: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<PostgresEvolutionRecordRow>, PersistenceError> {
        let rows = crate::dbm::postgres_query!(
            "SELECT id, tenant, record_type, status, created_by, derived_from, payload, timestamp \
             FROM evolution_records \
             WHERE tenant = $1 \
               AND ($2::text IS NULL OR record_type = $2) \
               AND ($3::text IS NULL OR status = $3) \
             ORDER BY timestamp DESC",
        )
        .bind(tenant)
        .bind(record_type)
        .bind(status)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows.into_iter().map(row_to_evolution_record).collect())
    }

    pub async fn list_ranked_insights(
        &self,
        tenant: &str,
    ) -> Result<Vec<PostgresEvolutionRecordRow>, PersistenceError> {
        let mut rows = self
            .list_evolution_records(tenant, Some("Insight"), None)
            .await?;
        rows.sort_by(|a, b| {
            let score_a = serde_json::from_str::<serde_json::Value>(&a.data)
                .ok()
                .and_then(|value| value.get("priority_score").and_then(|score| score.as_f64()))
                .unwrap_or(0.0);
            let score_b = serde_json::from_str::<serde_json::Value>(&b.data)
                .ok()
                .and_then(|value| value.get("priority_score").and_then(|score| score.as_f64()))
                .unwrap_or(0.0);
            score_b
                .partial_cmp(&score_a)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(rows)
    }
}
