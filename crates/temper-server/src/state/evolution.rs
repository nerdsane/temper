//! Backend-neutral evolution record access.
//!
//! Dispatches to whichever durable metadata backend is available so that
//! observe endpoints work regardless of the configured event store.

use temper_store_turso::EvolutionRecordRow;
use tracing::instrument;

use crate::storage::EvolutionRecordWrite;

use super::ServerState;

impl ServerState {
    /// List evolution records from the first available backend.
    #[instrument(skip_all, fields(otel.name = "evolution.list_records", record_type, status))]
    pub async fn list_evolution_records(
        &self,
        tenant: &str,
        record_type: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<EvolutionRecordRow>, String> {
        if let Some(store) = self.metadata_store_for_tenant(tenant).await {
            let rows = store
                .list_evolution_records(tenant, record_type, status)
                .await
                .map_err(|e| {
                    tracing::warn!(
                        backend = store.backend_name(),
                        record_type,
                        status,
                        error = %e,
                        "evolution.store.read"
                    );
                    e.to_string()
                })?;
            tracing::info!(
                backend = store.backend_name(),
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
                .list_records_generic(tenant, record_type, status)
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
        tenant: &str,
        id: &str,
    ) -> Result<Option<EvolutionRecordRow>, String> {
        if let Some(store) = self.metadata_store_for_tenant(tenant).await {
            let row = store
                .get_evolution_record(tenant, id)
                .await
                .map_err(|e| {
                    tracing::warn!(backend = store.backend_name(), record_id = id, error = %e, "evolution.store.read");
                    e.to_string()
                })?;
            tracing::info!(
                backend = store.backend_name(),
                record_id = id,
                found = row.is_some(),
                "evolution.record.get"
            );
            return Ok(row);
        }

        if let Some(pg) = &self.pg_record_store {
            let row = pg.get_record_generic(tenant, id).await.map_err(|e| {
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
    pub async fn list_ranked_insights(
        &self,
        tenant: &str,
    ) -> Result<Vec<EvolutionRecordRow>, String> {
        if let Some(store) = self.metadata_store_for_tenant(tenant).await {
            let rows = store.list_ranked_insights(tenant).await.map_err(|e| {
                tracing::warn!(backend = store.backend_name(), error = %e, "evolution.store.read");
                e.to_string()
            })?;
            tracing::info!(
                backend = store.backend_name(),
                count = rows.len(),
                "evolution.insight"
            );
            return Ok(rows);
        }

        if let Some(pg) = &self.pg_record_store {
            let rows = pg.list_ranked_insights_generic(tenant).await.map_err(|e| {
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
    #[instrument(skip_all, fields(otel.name = "evolution.insert_record"))]
    pub async fn insert_evolution_record(
        &self,
        record: EvolutionRecordWrite<'_>,
    ) -> Result<(), String> {
        let EvolutionRecordWrite {
            tenant,
            id,
            record_type,
            status,
            created_by,
            derived_from,
            data_json,
        } = record;
        if let Some(store) = self.metadata_store_for_tenant(tenant).await {
            store
                .insert_evolution_record(EvolutionRecordWrite {
                    tenant,
                    id,
                    record_type,
                    status,
                    created_by,
                    derived_from,
                    data_json,
                })
                .await
                .map_err(|e| {
                    tracing::warn!(
                        backend = store.backend_name(),
                        record_id = id,
                        record_type,
                        status,
                        error = %e,
                        "evolution.store.write"
                    );
                    e.to_string()
                })?;
            tracing::info!(
                backend = store.backend_name(),
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
            pg.insert_record_generic(temper_evolution::GenericEvolutionRecordInsert {
                tenant,
                id,
                record_type,
                status,
                created_by,
                derived_from,
                data_json,
            })
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
        tenant: row.tenant,
        record_type: row.record_type,
        status: row.status,
        created_by: row.created_by,
        derived_from: row.derived_from,
        data: row.data,
        timestamp: row.timestamp,
    }
}
