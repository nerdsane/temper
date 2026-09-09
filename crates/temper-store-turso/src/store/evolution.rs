//! Feature requests, evolution records, and design-time events.

use libsql::params;
use temper_runtime::persistence::{PersistenceError, storage_error};
use tracing::instrument;

use super::{DesignTimeEventRow, EvolutionRecordRow, FeatureRequestRow, TursoEventStore};
use crate::TursoEvolutionRecordInsert;
use crate::metrics::TursoQueryTimer;

// -----------------------------------------------------------------------
// Feature request CRUD
// -----------------------------------------------------------------------

impl TursoEventStore {
    /// Upsert a feature request.
    #[allow(clippy::too_many_arguments)]
    #[instrument(skip_all, fields(tenant, id, otel.name = "turso.upsert_feature_request"))]
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
        let _query_timer = TursoQueryTimer::start("turso.upsert_feature_request");
        let conn = self.configured_connection().await?;
        let affected = conn.execute(
            "INSERT INTO feature_requests (id, tenant, category, description, frequency, trajectory_refs, disposition, developer_notes, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, datetime('now')) \
             ON CONFLICT(id) DO UPDATE SET \
                 category = excluded.category, description = excluded.description, \
                 frequency = excluded.frequency, trajectory_refs = excluded.trajectory_refs, \
                 disposition = excluded.disposition, developer_notes = excluded.developer_notes, \
                 updated_at = datetime('now') \
             WHERE feature_requests.tenant = excluded.tenant",
            params![id, tenant, category, description, frequency, trajectory_refs_json, disposition, developer_notes],
        )
        .await
        .map_err(storage_error)?;
        if affected == 0 {
            return Err(PersistenceError::Storage(format!(
                "feature request '{id}' is owned by another tenant"
            )));
        }
        Ok(())
    }

    /// List feature requests with optional disposition filter.
    #[instrument(skip_all, fields(tenant, otel.name = "turso.list_feature_requests"))]
    pub async fn list_feature_requests(
        &self,
        tenant: &str,
        disposition: Option<&str>,
    ) -> Result<Vec<FeatureRequestRow>, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.list_feature_requests");
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT id, tenant, category, description, frequency, trajectory_refs, disposition, developer_notes, created_at, updated_at \
                 FROM feature_requests \
                 WHERE tenant = ?1 AND (?2 IS NULL OR disposition = ?2) \
                 ORDER BY frequency DESC, created_at DESC",
                params![tenant, disposition],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(FeatureRequestRow {
                id: row.get::<String>(0).map_err(storage_error)?,
                tenant: row.get::<String>(1).map_err(storage_error)?,
                category: row.get::<String>(2).map_err(storage_error)?,
                description: row.get::<String>(3).map_err(storage_error)?,
                frequency: row.get::<i64>(4).map_err(storage_error)?,
                trajectory_refs: row.get::<String>(5).map_err(storage_error)?,
                disposition: row.get::<String>(6).map_err(storage_error)?,
                developer_notes: row.get::<Option<String>>(7).map_err(storage_error)?,
                created_at: row.get::<String>(8).map_err(storage_error)?,
                updated_at: row.get::<String>(9).map_err(storage_error)?,
            });
        }
        Ok(out)
    }

    /// Update a feature request's disposition and developer notes.
    #[instrument(skip_all, fields(tenant, id, otel.name = "turso.update_feature_request"))]
    pub async fn update_feature_request(
        &self,
        tenant: &str,
        id: &str,
        disposition: &str,
        developer_notes: Option<&str>,
    ) -> Result<bool, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.update_feature_request");
        let conn = self.configured_connection().await?;
        let affected = conn
            .execute(
                "UPDATE feature_requests SET disposition = ?3, developer_notes = ?4, updated_at = datetime('now') \
                 WHERE tenant = ?1 AND id = ?2",
                params![tenant, id, disposition, developer_notes],
            )
            .await
            .map_err(storage_error)?;
        Ok(affected > 0)
    }

    // -----------------------------------------------------------------------
    // Evolution record CRUD
    // -----------------------------------------------------------------------

    /// Insert an evolution record.
    #[instrument(
        skip_all,
        fields(
            tenant = record.tenant,
            id = record.id,
            record_type = record.record_type,
            otel.name = "turso.insert_evolution_record"
        )
    )]
    pub async fn insert_evolution_record(
        &self,
        record: TursoEvolutionRecordInsert<'_>,
    ) -> Result<(), PersistenceError> {
        let TursoEvolutionRecordInsert {
            tenant,
            id,
            record_type,
            status,
            created_by,
            derived_from,
            data_json,
        } = record;
        let _query_timer = TursoQueryTimer::start("turso.insert_evolution_record");
        let conn = self.configured_connection().await?;
        let execute_res = conn
            .execute(
            "INSERT INTO evolution_records (id, tenant, record_type, status, created_by, derived_from, data, timestamp) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, datetime('now'))",
            params![id, tenant, record_type, status, created_by, derived_from, data_json],
        )
        .await
        .map_err(storage_error);
        if let Err(ref error) = execute_res {
            tracing::warn!(
                record_id = id,
                record_type,
                status,
                created_by,
                derived_from,
                error = %error,
                "evolution.store.write"
            );
        }
        execute_res?;
        tracing::info!(
            record_id = id,
            record_type,
            status,
            created_by,
            derived_from,
            "evolution.store.write"
        );
        Ok(())
    }

    /// Get a single evolution record by ID.
    #[instrument(skip_all, fields(tenant, id, otel.name = "turso.get_evolution_record"))]
    pub async fn get_evolution_record(
        &self,
        tenant: &str,
        id: &str,
    ) -> Result<Option<EvolutionRecordRow>, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.get_evolution_record");
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT id, tenant, record_type, status, created_by, derived_from, data, timestamp \
                 FROM evolution_records WHERE tenant = ?1 AND id = ?2",
                params![tenant, id],
            )
            .await
            .map_err(storage_error)?;

        let Some(row) = rows.next().await.map_err(storage_error)? else {
            tracing::debug!(record_id = id, found = false, "evolution.store.read");
            return Ok(None);
        };
        let parsed = Self::row_to_evolution_record(&row)?;
        tracing::info!(
            record_id = id,
            record_type = parsed.record_type.as_str(),
            status = parsed.status.as_str(),
            found = true,
            "evolution.store.read"
        );
        Ok(Some(parsed))
    }

    /// List evolution records with optional type and status filters.
    #[instrument(skip_all, fields(tenant, otel.name = "turso.list_evolution_records"))]
    pub async fn list_evolution_records(
        &self,
        tenant: &str,
        record_type: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<EvolutionRecordRow>, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.list_evolution_records");
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT id, tenant, record_type, status, created_by, derived_from, data, timestamp \
                 FROM evolution_records \
                 WHERE tenant = ?1 \
                   AND (?2 IS NULL OR record_type = ?2) \
                   AND (?3 IS NULL OR status = ?3) \
                 ORDER BY timestamp DESC",
                params![tenant, record_type, status],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(Self::row_to_evolution_record(&row)?);
        }
        tracing::info!(
            record_type,
            status,
            count = out.len(),
            "evolution.store.read"
        );
        Ok(out)
    }

    /// List ranked insights (Insight type, sorted by priority_score in data).
    #[instrument(skip_all, fields(tenant, otel.name = "turso.list_ranked_insights"))]
    pub async fn list_ranked_insights(
        &self,
        tenant: &str,
    ) -> Result<Vec<EvolutionRecordRow>, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.list_ranked_insights");
        let mut out = self
            .list_evolution_records(tenant, Some("Insight"), None)
            .await?;
        // Sort by priority_score descending (extracted from JSON data).
        out.sort_by(|a, b| {
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
        tracing::info!(count = out.len(), "evolution.insight");
        Ok(out)
    }

    /// Parse an evolution record row.
    pub(super) fn row_to_evolution_record(
        row: &libsql::Row,
    ) -> Result<EvolutionRecordRow, PersistenceError> {
        Ok(EvolutionRecordRow {
            id: row.get::<String>(0).map_err(storage_error)?,
            tenant: row.get::<String>(1).map_err(storage_error)?,
            record_type: row.get::<String>(2).map_err(storage_error)?,
            status: row.get::<String>(3).map_err(storage_error)?,
            created_by: row.get::<String>(4).map_err(storage_error)?,
            derived_from: row.get::<Option<String>>(5).map_err(storage_error)?,
            data: row.get::<String>(6).map_err(storage_error)?,
            timestamp: row.get::<String>(7).map_err(storage_error)?,
        })
    }

    // -----------------------------------------------------------------------
    // Design-time event CRUD
    // -----------------------------------------------------------------------

    /// Insert a design-time event.
    #[allow(clippy::too_many_arguments)]
    #[instrument(skip_all, fields(tenant, entity_type, otel.name = "turso.insert_design_time_event"))]
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
        let _query_timer = TursoQueryTimer::start("turso.insert_design_time_event");
        let conn = self.configured_connection().await?;
        conn.execute(
            "INSERT INTO design_time_events (kind, entity_type, tenant, summary, level, passed, step_number, total_steps) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![kind, entity_type, tenant, summary, level, passed.map(|b| b as i64), step_number, total_steps],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// List recent design-time events for a tenant.
    #[instrument(skip_all, fields(otel.name = "turso.list_design_time_events"))]
    pub async fn list_design_time_events(
        &self,
        tenant: Option<&str>,
        limit: i64,
    ) -> Result<Vec<DesignTimeEventRow>, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.list_design_time_events");
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT id, kind, entity_type, tenant, summary, level, passed, step_number, total_steps, created_at \
                 FROM design_time_events \
                 WHERE (?1 IS NULL OR tenant = ?1) \
                 ORDER BY created_at DESC \
                 LIMIT ?2",
                params![tenant, limit],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(DesignTimeEventRow {
                id: row.get::<i64>(0).map_err(storage_error)?,
                kind: row.get::<String>(1).map_err(storage_error)?,
                entity_type: row.get::<String>(2).map_err(storage_error)?,
                tenant: row.get::<String>(3).map_err(storage_error)?,
                summary: row.get::<String>(4).map_err(storage_error)?,
                level: row.get::<Option<String>>(5).map_err(storage_error)?,
                passed: row
                    .get::<Option<i64>>(6)
                    .map_err(storage_error)?
                    .map(|v| v != 0),
                step_number: row.get::<Option<i64>>(7).map_err(storage_error)?,
                total_steps: row.get::<Option<i64>>(8).map_err(storage_error)?,
                created_at: row.get::<String>(9).map_err(storage_error)?,
            });
        }
        Ok(out)
    }
}
