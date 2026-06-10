//! Feature requests, evolution records, and design-time events.

use sqlx::Row;
use temper_runtime::persistence::PersistenceError;

use super::{parse_json, storage_error};
use crate::PostgresEventStore;

#[derive(Debug, Clone, serde::Serialize)]
pub struct PostgresFeatureRequestRow {
    pub id: String,
    pub category: String,
    pub description: String,
    pub frequency: i64,
    pub trajectory_refs: String,
    pub disposition: String,
    pub developer_notes: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PostgresEvolutionRecordRow {
    pub id: String,
    pub record_type: String,
    pub status: String,
    pub created_by: String,
    pub derived_from: Option<String>,
    pub data: String,
    pub timestamp: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PostgresDesignTimeEventRow {
    pub id: i64,
    pub kind: String,
    pub entity_type: String,
    pub tenant: String,
    pub summary: String,
    pub level: Option<String>,
    pub passed: Option<bool>,
    pub step_number: Option<i64>,
    pub total_steps: Option<i64>,
    pub created_at: String,
}

impl PostgresEventStore {
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_feature_request(
        &self,
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
             (id, category, description, frequency, trajectory_refs, disposition, developer_notes, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, now()) \
             ON CONFLICT (id) DO UPDATE SET \
                 category = EXCLUDED.category, description = EXCLUDED.description, frequency = EXCLUDED.frequency, \
                 trajectory_refs = EXCLUDED.trajectory_refs, disposition = EXCLUDED.disposition, \
                 developer_notes = EXCLUDED.developer_notes, updated_at = now()",
        )
        .bind(id)
        .bind(category)
        .bind(description)
        .bind(frequency)
        .bind(trajectory_refs)
        .bind(disposition)
        .bind(developer_notes)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    pub async fn list_feature_requests(
        &self,
        disposition: Option<&str>,
    ) -> Result<Vec<PostgresFeatureRequestRow>, PersistenceError> {
        let rows = crate::dbm::postgres_query!(
            "SELECT id, category, description, frequency, trajectory_refs, disposition, developer_notes, created_at, updated_at \
             FROM feature_requests \
             WHERE ($1::text IS NULL OR disposition = $1) \
             ORDER BY frequency DESC, created_at DESC",
        )
        .bind(disposition)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows.into_iter().map(row_to_feature_request).collect())
    }

    pub async fn update_feature_request(
        &self,
        id: &str,
        disposition: &str,
        developer_notes: Option<&str>,
    ) -> Result<bool, PersistenceError> {
        let result = crate::dbm::postgres_query!(
            "UPDATE feature_requests SET disposition = $2, developer_notes = $3, updated_at = now() \
             WHERE id = $1",
        )
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
        id: &str,
        record_type: &str,
        status: &str,
        created_by: &str,
        derived_from: Option<&str>,
        data_json: &str,
    ) -> Result<(), PersistenceError> {
        let payload = parse_json(data_json)?;
        crate::dbm::postgres_query!(
            "INSERT INTO evolution_records (id, record_type, status, created_by, derived_from, payload, timestamp) \
             VALUES ($1, $2, $3, $4, $5, $6, now())",
        )
        .bind(id)
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
        id: &str,
    ) -> Result<Option<PostgresEvolutionRecordRow>, PersistenceError> {
        let row = crate::dbm::postgres_query!(
            "SELECT id, record_type, status, created_by, derived_from, payload, timestamp \
             FROM evolution_records WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(row.map(row_to_evolution_record))
    }

    pub async fn list_evolution_records(
        &self,
        record_type: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<PostgresEvolutionRecordRow>, PersistenceError> {
        let rows = crate::dbm::postgres_query!(
            "SELECT id, record_type, status, created_by, derived_from, payload, timestamp \
             FROM evolution_records \
             WHERE ($1::text IS NULL OR record_type = $1) \
               AND ($2::text IS NULL OR status = $2) \
             ORDER BY timestamp DESC",
        )
        .bind(record_type)
        .bind(status)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows.into_iter().map(row_to_evolution_record).collect())
    }

    pub async fn list_ranked_insights(
        &self,
    ) -> Result<Vec<PostgresEvolutionRecordRow>, PersistenceError> {
        let mut rows = self.list_evolution_records(Some("Insight"), None).await?;
        rows.sort_by(|a, b| {
            let score_a = serde_json::from_str::<serde_json::Value>(&a.data)
                .ok()
                .and_then(|v| v.get("priority_score").and_then(|s| s.as_f64()))
                .unwrap_or(0.0);
            let score_b = serde_json::from_str::<serde_json::Value>(&b.data)
                .ok()
                .and_then(|v| v.get("priority_score").and_then(|s| s.as_f64()))
                .unwrap_or(0.0);
            score_b
                .partial_cmp(&score_a)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(rows)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn insert_design_time_event(
        &self,
        kind: &str,
        entity_type: &str,
        tenant: &str,
        summary: &str,
        level: Option<&str>,
        passed: Option<bool>,
        step_number: Option<i64>,
        total_steps: Option<i64>,
    ) -> Result<(), PersistenceError> {
        crate::dbm::postgres_query!(
            "INSERT INTO design_time_events \
             (kind, entity_type, tenant, summary, level, passed, step_number, total_steps) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(kind)
        .bind(entity_type)
        .bind(tenant)
        .bind(summary)
        .bind(level)
        .bind(passed)
        .bind(step_number.map(|value| value as i16))
        .bind(total_steps.map(|value| value as i16))
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    pub async fn list_design_time_events(
        &self,
        tenant: Option<&str>,
        limit: i64,
    ) -> Result<Vec<PostgresDesignTimeEventRow>, PersistenceError> {
        let rows = crate::dbm::postgres_query!(
            "SELECT id, kind, entity_type, tenant, summary, level, passed, step_number, total_steps, created_at \
             FROM design_time_events \
             WHERE ($1::text IS NULL OR tenant = $1) \
             ORDER BY created_at DESC \
             LIMIT $2",
        )
        .bind(tenant)
        .bind(limit)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows.into_iter().map(row_to_design_time_event).collect())
    }
}

fn row_to_feature_request(row: sqlx::postgres::PgRow) -> PostgresFeatureRequestRow {
    let trajectory_refs: serde_json::Value = row.get("trajectory_refs");
    let created_at: chrono::DateTime<chrono::Utc> = row.get("created_at");
    let updated_at: chrono::DateTime<chrono::Utc> = row.get("updated_at");
    PostgresFeatureRequestRow {
        id: row.get("id"),
        category: row.get("category"),
        description: row.get("description"),
        frequency: row.get("frequency"),
        trajectory_refs: trajectory_refs.to_string(),
        disposition: row.get("disposition"),
        developer_notes: row.get("developer_notes"),
        created_at: created_at.to_rfc3339(),
        updated_at: updated_at.to_rfc3339(),
    }
}

fn row_to_evolution_record(row: sqlx::postgres::PgRow) -> PostgresEvolutionRecordRow {
    let payload: serde_json::Value = row.get("payload");
    let timestamp: chrono::DateTime<chrono::Utc> = row.get("timestamp");
    PostgresEvolutionRecordRow {
        id: row.get("id"),
        record_type: row.get("record_type"),
        status: row.get("status"),
        created_by: row.get("created_by"),
        derived_from: row.get("derived_from"),
        data: payload.to_string(),
        timestamp: timestamp.to_rfc3339(),
    }
}

fn row_to_design_time_event(row: sqlx::postgres::PgRow) -> PostgresDesignTimeEventRow {
    let created_at: chrono::DateTime<chrono::Utc> = row.get("created_at");
    let step_number: Option<i16> = row.get("step_number");
    let total_steps: Option<i16> = row.get("total_steps");
    PostgresDesignTimeEventRow {
        id: row.get("id"),
        kind: row.get("kind"),
        entity_type: row.get("entity_type"),
        tenant: row.get("tenant"),
        summary: row.get("summary"),
        level: row.get("level"),
        passed: row.get("passed"),
        step_number: step_number.map(i64::from),
        total_steps: total_steps.map(i64::from),
        created_at: created_at.to_rfc3339(),
    }
}
