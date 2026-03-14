//! Backend-neutral evolution record access.
//!
//! Dispatches to whichever backend is available (Turso → Postgres) so that
//! observe endpoints work regardless of the configured event store.

use temper_store_turso::EvolutionRecordRow;
use tracing::instrument;

use super::ServerState;

impl ServerState {
    /// List evolution records from the first available backend.
    #[instrument(skip_all, fields(otel.name = "evolution.list_records", record_type, status))]
    pub async fn list_evolution_records(
        &self,
        record_type: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<EvolutionRecordRow>, String> {
        // Prefer Turso when available.
        if let Some(turso) = self.persistent_store() {
            let rows = turso
                .list_evolution_records(record_type, status)
                .await
                .map_err(|e| {
                    tracing::warn!(
                        backend = "turso",
                        record_type,
                        status,
                        error = %e,
                        "evolution.store.read"
                    );
                    e.to_string()
                })?;
            tracing::info!(
                backend = "turso",
                record_type,
                status,
                count = rows.len(),
                "evolution.record.list"
            );
            return Ok(rows);
        }

        // Fall through to Postgres.
        if let Some(pg) = &self.pg_record_store {
            let rows = pg
                .list_records_generic(record_type, status)
                .await
                .map_err(|e| {
                    tracing::warn!(
                        backend = "postgres",
                        record_type,
                        status,
                        error = %e,
                        "evolution.store.read"
                    );
                    e.to_string()
                })?;
            let mapped: Vec<EvolutionRecordRow> = rows.into_iter().map(pg_row_to_turso).collect();
            tracing::info!(
                backend = "postgres",
                record_type,
                status,
                count = mapped.len(),
                "evolution.record.list"
            );
            return Ok(mapped);
        }

        tracing::warn!(record_type, status, "evolution.store.unavailable");
        Ok(Vec::new())
    }

    /// Get a single evolution record by ID from the first available backend.
    #[instrument(skip_all, fields(otel.name = "evolution.get_record", id))]
    pub async fn get_evolution_record(
        &self,
        id: &str,
    ) -> Result<Option<EvolutionRecordRow>, String> {
        if let Some(turso) = self.persistent_store() {
            let row = turso
                .get_evolution_record(id)
                .await
                .map_err(|e| {
                    tracing::warn!(backend = "turso", record_id = id, error = %e, "evolution.store.read");
                    e.to_string()
                })?;
            tracing::info!(
                backend = "turso",
                record_id = id,
                found = row.is_some(),
                "evolution.record.get"
            );
            return Ok(row);
        }

        if let Some(pg) = &self.pg_record_store {
            let row = pg.get_record_generic(id).await.map_err(|e| {
                tracing::warn!(backend = "postgres", record_id = id, error = %e, "evolution.store.read");
                e.to_string()
            })?;
            let mapped = row.map(pg_row_to_turso);
            tracing::info!(
                backend = "postgres",
                record_id = id,
                found = mapped.is_some(),
                "evolution.record.get"
            );
            return Ok(mapped);
        }

        tracing::warn!(record_id = id, "evolution.store.unavailable");
        Ok(None)
    }

    /// List ranked insights (I-Records) from the first available backend.
    #[instrument(skip_all, fields(otel.name = "evolution.list_ranked_insights"))]
    pub async fn list_ranked_insights(&self) -> Result<Vec<EvolutionRecordRow>, String> {
        if let Some(turso) = self.persistent_store() {
            let rows = turso.list_ranked_insights().await.map_err(|e| {
                tracing::warn!(backend = "turso", error = %e, "evolution.store.read");
                e.to_string()
            })?;
            tracing::info!(backend = "turso", count = rows.len(), "evolution.insight");
            return Ok(rows);
        }

        if let Some(pg) = &self.pg_record_store {
            let rows = pg.list_ranked_insights_generic().await.map_err(|e| {
                tracing::warn!(backend = "postgres", error = %e, "evolution.store.read");
                e.to_string()
            })?;
            let mapped: Vec<EvolutionRecordRow> = rows.into_iter().map(pg_row_to_turso).collect();
            tracing::info!(
                backend = "postgres",
                count = mapped.len(),
                "evolution.insight"
            );
            return Ok(mapped);
        }

        tracing::warn!("evolution.store.unavailable");
        Ok(Vec::new())
    }

    /// Insert a generic evolution record into the first available backend.
    #[instrument(skip_all, fields(otel.name = "evolution.insert_record", id, record_type, status))]
    pub async fn insert_evolution_record(
        &self,
        id: &str,
        record_type: &str,
        status: &str,
        created_by: &str,
        derived_from: Option<&str>,
        data_json: &str,
    ) -> Result<(), String> {
        if let Some(turso) = self.persistent_store() {
            turso
                .insert_evolution_record(
                    id,
                    record_type,
                    status,
                    created_by,
                    derived_from,
                    data_json,
                )
                .await
                .map_err(|e| {
                    tracing::warn!(
                        backend = "turso",
                        record_id = id,
                        record_type,
                        status,
                        error = %e,
                        "evolution.store.write"
                    );
                    e.to_string()
                })?;
            tracing::info!(
                backend = "turso",
                record_id = id,
                record_type,
                status,
                created_by,
                derived_from,
                "evolution.record.create"
            );
            return Ok(());
        }

        if let Some(pg) = &self.pg_record_store {
            pg.insert_record_generic(id, record_type, status, created_by, derived_from, data_json)
                .await
                .map_err(|e| {
                    tracing::warn!(
                        backend = "postgres",
                        record_id = id,
                        record_type,
                        status,
                        error = %e,
                        "evolution.store.write"
                    );
                    e.to_string()
                })?;
            tracing::info!(
                backend = "postgres",
                record_id = id,
                record_type,
                status,
                created_by,
                derived_from,
                "evolution.record.create"
            );
            return Ok(());
        }

        tracing::warn!(
            record_id = id,
            record_type,
            status,
            "evolution.store.unavailable"
        );
        Err("no evolution store configured".to_string())
    }
}

/// Convert a Postgres `GenericEvolutionRow` to the Turso `EvolutionRecordRow` format.
fn pg_row_to_turso(row: temper_evolution::GenericEvolutionRow) -> EvolutionRecordRow {
    EvolutionRecordRow {
        id: row.id,
        record_type: row.record_type,
        status: row.status,
        created_by: row.created_by,
        derived_from: row.derived_from,
        data: row.data,
        timestamp: row.timestamp,
    }
}
