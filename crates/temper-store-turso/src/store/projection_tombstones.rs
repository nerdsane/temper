//! Sequence-aware durable query projection removal.

use libsql::{TransactionBehavior, params};
use temper_runtime::persistence::{PersistenceError, storage_error};
use temper_runtime::scheduler::sim_now;

use super::TursoEventStore;
use super::write_gate::WritePriority;

impl TursoEventStore {
    /// Record a durable removal high-water mark and delete no projection newer
    /// than that mark in the same immediate transaction.
    pub async fn remove_query_projection_versioned(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        let _write_permit = self
            .acquire_write_permit(
                "turso.remove_query_projection_versioned",
                WritePriority::Low,
            )
            .await?;
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;
        let sequence_nr = i64::try_from(sequence_nr).unwrap_or(i64::MAX);
        let deleted_at = sim_now().to_rfc3339();

        tx.execute(
            "INSERT INTO query_projection_tombstones \
             (tenant, entity_type, entity_id, sequence_nr, deleted_at) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(tenant, entity_type, entity_id) DO UPDATE SET \
                 sequence_nr = MAX(query_projection_tombstones.sequence_nr, excluded.sequence_nr), \
                 deleted_at = CASE \
                     WHEN query_projection_tombstones.sequence_nr <= excluded.sequence_nr THEN excluded.deleted_at \
                     ELSE query_projection_tombstones.deleted_at \
                 END",
            params![tenant, entity_type, entity_id, sequence_nr, deleted_at],
        )
        .await
        .map_err(storage_error)?;
        tx.execute(
            "DELETE FROM entity_catalog \
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 \
               AND sequence_nr <= ?4",
            params![tenant, entity_type, entity_id, sequence_nr],
        )
        .await
        .map_err(storage_error)?;
        tx.execute(
            "DELETE FROM entity_field_index \
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 \
               AND NOT EXISTS ( \
                   SELECT 1 FROM entity_catalog catalog \
                    WHERE catalog.tenant = ?1 AND catalog.entity_type = ?2 AND catalog.entity_id = ?3 \
               )",
            params![tenant, entity_type, entity_id],
        )
        .await
        .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)
    }
}
