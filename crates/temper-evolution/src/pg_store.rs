//! Postgres-backed record store for evolution records.
//!
//! Uses a single `evolution_records` table with a JSONB `payload` column
//! to store all five record types. Indexes on `(record_type, status)` and
//! `(derived_from)` enable efficient chain traversal and status queries.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::records::*;

/// Error type for Postgres record store operations.
#[derive(Debug, thiserror::Error)]
pub enum PgRecordStoreError {
    /// Database query failed.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// JSON serialization/deserialization failed.
    #[error("serialization error: {0}")]
    Serialization(String),
}

/// A row from the `evolution_records` table.
#[derive(Debug, sqlx::FromRow)]
struct EvolutionRow {
    id: String,
    record_type: String,
    status: String,
    created_by: String,
    derived_from: Option<String>,
    timestamp: DateTime<Utc>,
    payload: serde_json::Value,
}

/// Postgres-backed evolution record store.
///
/// All five record types are stored in a single `evolution_records` table.
/// The record-type-specific data lives in the `payload` JSONB column.
#[derive(Clone)]
pub struct PostgresRecordStore {
    pool: PgPool,
}

impl PostgresRecordStore {
    /// Create a new Postgres record store.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Run the migration to create the `evolution_records` table.
    pub async fn migrate(&self) -> Result<(), PgRecordStoreError> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS evolution_records (
                id          TEXT PRIMARY KEY,
                record_type TEXT NOT NULL,
                status      TEXT NOT NULL,
                created_by  TEXT NOT NULL,
                derived_from TEXT,
                timestamp   TIMESTAMPTZ NOT NULL,
                payload     JSONB NOT NULL
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_evolution_records_type_status
            ON evolution_records (record_type, status)
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_evolution_records_derived_from
            ON evolution_records (derived_from)
            "#,
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    // -- Insert --

    /// Insert an observation record.
    pub async fn insert_observation(
        &self,
        record: &ObservationRecord,
    ) -> Result<(), PgRecordStoreError> {
        let payload = serde_json::to_value(record)
            .map_err(|e| PgRecordStoreError::Serialization(e.to_string()))?;
        self.insert_row(&record.header, &payload).await
    }

    /// Insert a problem record.
    pub async fn insert_problem(
        &self,
        record: &ProblemRecord,
    ) -> Result<(), PgRecordStoreError> {
        let payload = serde_json::to_value(record)
            .map_err(|e| PgRecordStoreError::Serialization(e.to_string()))?;
        self.insert_row(&record.header, &payload).await
    }

    /// Insert an analysis record.
    pub async fn insert_analysis(
        &self,
        record: &AnalysisRecord,
    ) -> Result<(), PgRecordStoreError> {
        let payload = serde_json::to_value(record)
            .map_err(|e| PgRecordStoreError::Serialization(e.to_string()))?;
        self.insert_row(&record.header, &payload).await
    }

    /// Insert a decision record.
    pub async fn insert_decision(
        &self,
        record: &DecisionRecord,
    ) -> Result<(), PgRecordStoreError> {
        let payload = serde_json::to_value(record)
            .map_err(|e| PgRecordStoreError::Serialization(e.to_string()))?;
        self.insert_row(&record.header, &payload).await
    }

    /// Insert an insight record.
    pub async fn insert_insight(
        &self,
        record: &InsightRecord,
    ) -> Result<(), PgRecordStoreError> {
        let payload = serde_json::to_value(record)
            .map_err(|e| PgRecordStoreError::Serialization(e.to_string()))?;
        self.insert_row(&record.header, &payload).await
    }

    // -- Query --

    /// Retrieve an observation record by ID.
    pub async fn get_observation(
        &self,
        id: &str,
    ) -> Result<Option<ObservationRecord>, PgRecordStoreError> {
        self.get_record(id, "Observation").await
    }

    /// Retrieve a problem record by ID.
    pub async fn get_problem(
        &self,
        id: &str,
    ) -> Result<Option<ProblemRecord>, PgRecordStoreError> {
        self.get_record(id, "Problem").await
    }

    /// Retrieve an analysis record by ID.
    pub async fn get_analysis(
        &self,
        id: &str,
    ) -> Result<Option<AnalysisRecord>, PgRecordStoreError> {
        self.get_record(id, "Analysis").await
    }

    /// Retrieve a decision record by ID.
    pub async fn get_decision(
        &self,
        id: &str,
    ) -> Result<Option<DecisionRecord>, PgRecordStoreError> {
        self.get_record(id, "Decision").await
    }

    /// Retrieve an insight record by ID.
    pub async fn get_insight(
        &self,
        id: &str,
    ) -> Result<Option<InsightRecord>, PgRecordStoreError> {
        self.get_record(id, "Insight").await
    }

    /// Get all open observations (status = 'Open').
    pub async fn open_observations(
        &self,
    ) -> Result<Vec<ObservationRecord>, PgRecordStoreError> {
        let rows: Vec<EvolutionRow> = sqlx::query_as(
            "SELECT id, record_type, status, created_by, derived_from, timestamp, payload \
             FROM evolution_records WHERE record_type = 'Observation' AND status = 'Open' \
             ORDER BY timestamp DESC",
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                serde_json::from_value(row.payload)
                    .map_err(|e| PgRecordStoreError::Serialization(e.to_string()))
            })
            .collect()
    }

    /// Get all open insights sorted by priority score (highest first).
    pub async fn ranked_insights(
        &self,
    ) -> Result<Vec<InsightRecord>, PgRecordStoreError> {
        let rows: Vec<EvolutionRow> = sqlx::query_as(
            "SELECT id, record_type, status, created_by, derived_from, timestamp, payload \
             FROM evolution_records WHERE record_type = 'Insight' AND status = 'Open' \
             ORDER BY (payload->>'priority_score')::float8 DESC",
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                serde_json::from_value(row.payload)
                    .map_err(|e| PgRecordStoreError::Serialization(e.to_string()))
            })
            .collect()
    }

    /// Count records by type.
    pub async fn count(
        &self,
        record_type: RecordType,
    ) -> Result<i64, PgRecordStoreError> {
        let type_str = record_type_to_string(record_type);
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM evolution_records WHERE record_type = $1",
        )
        .bind(type_str)
        .fetch_one(&self.pool)
        .await?;

        Ok(row.0)
    }

    /// Update the status of a record.
    pub async fn update_status(
        &self,
        id: &str,
        new_status: RecordStatus,
    ) -> Result<bool, PgRecordStoreError> {
        let status_str = record_status_to_string(new_status);
        let result = sqlx::query(
            "UPDATE evolution_records SET status = $1, \
             payload = jsonb_set(payload, '{header,status}', to_jsonb($1::text)) \
             WHERE id = $2",
        )
        .bind(status_str)
        .bind(id)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Get all records derived from a given parent record.
    pub async fn get_derived_records(
        &self,
        parent_id: &str,
    ) -> Result<Vec<(String, RecordType, RecordStatus)>, PgRecordStoreError> {
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT id, record_type, status FROM evolution_records \
             WHERE derived_from = $1 ORDER BY timestamp",
        )
        .bind(parent_id)
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|(id, rt, st)| {
                let record_type = string_to_record_type(&rt)
                    .ok_or_else(|| PgRecordStoreError::Serialization(
                        format!("unknown record type: {rt}"),
                    ))?;
                let status = string_to_record_status(&st)
                    .ok_or_else(|| PgRecordStoreError::Serialization(
                        format!("unknown status: {st}"),
                    ))?;
                Ok((id, record_type, status))
            })
            .collect()
    }

    // -- Internal helpers --

    async fn insert_row(
        &self,
        header: &RecordHeader,
        payload: &serde_json::Value,
    ) -> Result<(), PgRecordStoreError> {
        let type_str = record_type_to_string(header.record_type);
        let status_str = record_status_to_string(header.status);

        sqlx::query(
            "INSERT INTO evolution_records \
             (id, record_type, status, created_by, derived_from, timestamp, payload) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&header.id)
        .bind(type_str)
        .bind(status_str)
        .bind(&header.created_by)
        .bind(header.derived_from.as_deref())
        .bind(header.timestamp)
        .bind(payload)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn get_record<T: serde::de::DeserializeOwned>(
        &self,
        id: &str,
        expected_type: &str,
    ) -> Result<Option<T>, PgRecordStoreError> {
        let row: Option<EvolutionRow> = sqlx::query_as(
            "SELECT id, record_type, status, created_by, derived_from, timestamp, payload \
             FROM evolution_records WHERE id = $1 AND record_type = $2",
        )
        .bind(id)
        .bind(expected_type)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some(r) => {
                let record: T = serde_json::from_value(r.payload)
                    .map_err(|e| PgRecordStoreError::Serialization(e.to_string()))?;
                Ok(Some(record))
            }
            None => Ok(None),
        }
    }
}

// -- String conversion helpers --

fn record_type_to_string(rt: RecordType) -> &'static str {
    match rt {
        RecordType::Observation => "Observation",
        RecordType::Problem => "Problem",
        RecordType::Analysis => "Analysis",
        RecordType::Decision => "Decision",
        RecordType::Insight => "Insight",
    }
}

fn record_status_to_string(status: RecordStatus) -> &'static str {
    match status {
        RecordStatus::Open => "Open",
        RecordStatus::Resolved => "Resolved",
        RecordStatus::Superseded => "Superseded",
        RecordStatus::Rejected => "Rejected",
    }
}

fn string_to_record_type(s: &str) -> Option<RecordType> {
    match s {
        "Observation" => Some(RecordType::Observation),
        "Problem" => Some(RecordType::Problem),
        "Analysis" => Some(RecordType::Analysis),
        "Decision" => Some(RecordType::Decision),
        "Insight" => Some(RecordType::Insight),
        _ => None,
    }
}

fn string_to_record_status(s: &str) -> Option<RecordStatus> {
    match s {
        "Open" => Some(RecordStatus::Open),
        "Resolved" => Some(RecordStatus::Resolved),
        "Superseded" => Some(RecordStatus::Superseded),
        "Rejected" => Some(RecordStatus::Rejected),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_type_roundtrip() {
        for rt in [
            RecordType::Observation,
            RecordType::Problem,
            RecordType::Analysis,
            RecordType::Decision,
            RecordType::Insight,
        ] {
            let s = record_type_to_string(rt);
            assert_eq!(string_to_record_type(s), Some(rt));
        }
    }

    #[test]
    fn test_record_status_roundtrip() {
        for st in [
            RecordStatus::Open,
            RecordStatus::Resolved,
            RecordStatus::Superseded,
            RecordStatus::Rejected,
        ] {
            let s = record_status_to_string(st);
            assert_eq!(string_to_record_status(s), Some(st));
        }
    }

    #[test]
    fn test_unknown_type_returns_none() {
        assert_eq!(string_to_record_type("Unknown"), None);
        assert_eq!(string_to_record_status("Unknown"), None);
    }

    #[test]
    fn test_observation_serialization_for_payload() {
        let record = ObservationRecord {
            header: RecordHeader::new(RecordType::Observation, "test"),
            source: "sentinel:latency".to_string(),
            classification: ObservationClass::Performance,
            evidence_query: "SELECT 1".to_string(),
            threshold_field: None,
            threshold_value: None,
            observed_value: Some(42.0),
            context: serde_json::json!({}),
        };

        let payload = serde_json::to_value(&record).expect("should serialize");
        let deserialized: ObservationRecord =
            serde_json::from_value(payload).expect("should deserialize");
        assert_eq!(deserialized.header.id, record.header.id);
        assert_eq!(deserialized.source, "sentinel:latency");
    }

    #[test]
    fn test_insight_serialization_for_payload() {
        let record = InsightRecord {
            header: RecordHeader::new(RecordType::Insight, "test"),
            category: InsightCategory::UnmetIntent,
            signal: InsightSignal {
                intent: "test".to_string(),
                volume: 100,
                success_rate: 0.5,
                trend: "stable".to_string(),
                growth_rate: None,
            },
            recommendation: "build it".to_string(),
            priority_score: 0.8,
        };

        let payload = serde_json::to_value(&record).expect("should serialize");
        let deserialized: InsightRecord =
            serde_json::from_value(payload).expect("should deserialize");
        assert_eq!(deserialized.priority_score, 0.8);
    }
}
