//! Backend-neutral evolution record access.
//!
//! Dispatches to whichever backend is available (Turso → Postgres) so that
//! observe endpoints work regardless of the configured event store.

use temper_store_turso::EvolutionRecordRow;

use super::ServerState;

impl ServerState {
    /// List evolution records from the first available backend.
    pub async fn list_evolution_records(
        &self,
        record_type: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<EvolutionRecordRow>, String> {
        // Prefer Turso when available.
        if let Some(turso) = self.persistent_store() {
            return turso
                .list_evolution_records(record_type, status)
                .await
                .map_err(|e| e.to_string());
        }

        // Fall through to Postgres.
        if let Some(pg) = &self.pg_record_store {
            let rows = pg
                .list_records_generic(record_type, status)
                .await
                .map_err(|e| e.to_string())?;
            return Ok(rows.into_iter().map(pg_row_to_turso).collect());
        }

        Ok(Vec::new())
    }

    /// Get a single evolution record by ID from the first available backend.
    pub async fn get_evolution_record(
        &self,
        id: &str,
    ) -> Result<Option<EvolutionRecordRow>, String> {
        if let Some(turso) = self.persistent_store() {
            return turso
                .get_evolution_record(id)
                .await
                .map_err(|e| e.to_string());
        }

        if let Some(pg) = &self.pg_record_store {
            let row = pg
                .get_record_generic(id)
                .await
                .map_err(|e| e.to_string())?;
            return Ok(row.map(pg_row_to_turso));
        }

        Ok(None)
    }

    /// List ranked insights (I-Records) from the first available backend.
    pub async fn list_ranked_insights(&self) -> Result<Vec<EvolutionRecordRow>, String> {
        if let Some(turso) = self.persistent_store() {
            return turso
                .list_ranked_insights()
                .await
                .map_err(|e| e.to_string());
        }

        if let Some(pg) = &self.pg_record_store {
            let rows = pg
                .list_ranked_insights_generic()
                .await
                .map_err(|e| e.to_string())?;
            return Ok(rows.into_iter().map(pg_row_to_turso).collect());
        }

        Ok(Vec::new())
    }

    /// Insert a generic evolution record into the first available backend.
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
            return turso
                .insert_evolution_record(id, record_type, status, created_by, derived_from, data_json)
                .await
                .map_err(|e| e.to_string());
        }

        if let Some(pg) = &self.pg_record_store {
            return pg
                .insert_record_generic(id, record_type, status, created_by, derived_from, data_json)
                .await
                .map_err(|e| e.to_string());
        }

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
