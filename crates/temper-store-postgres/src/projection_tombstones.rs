//! Sequence-aware durable query projection removal.

use sqlx::Acquire;
use temper_runtime::persistence::{PersistenceError, storage_error};

use crate::PostgresEventStore;

impl PostgresEventStore {
    /// Record a durable removal high-water mark and delete no projection newer
    /// than that mark in the same transaction.
    pub async fn remove_query_projection_versioned(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        let sequence_nr = i64::try_from(sequence_nr).unwrap_or(i64::MAX);
        let mut conn = self.pool().acquire().await.map_err(storage_error)?;
        let mut tx = conn.begin().await.map_err(storage_error)?;

        crate::dbm::postgres_query!(
            "INSERT INTO query_projection_tombstones \
             (tenant, entity_type, entity_id, sequence_nr, deleted_at) \
             VALUES ($1, $2, $3, $4, now()) \
             ON CONFLICT (tenant, entity_type, entity_id) DO UPDATE SET \
                 sequence_nr = GREATEST(query_projection_tombstones.sequence_nr, EXCLUDED.sequence_nr), \
                 deleted_at = CASE \
                     WHEN query_projection_tombstones.sequence_nr <= EXCLUDED.sequence_nr THEN now() \
                     ELSE query_projection_tombstones.deleted_at \
                 END",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind(sequence_nr)
        .execute(&mut *tx)
        .await
        .map_err(storage_error)?;

        crate::dbm::postgres_query!(
            "DELETE FROM entity_catalog \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
               AND sequence_nr <= $4",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind(sequence_nr)
        .execute(&mut *tx)
        .await
        .map_err(storage_error)?;

        crate::dbm::postgres_query!(
            "DELETE FROM entity_field_index fields \
             WHERE fields.tenant = $1 AND fields.entity_type = $2 AND fields.entity_id = $3 \
               AND NOT EXISTS ( \
                   SELECT 1 FROM entity_catalog catalog \
                    WHERE catalog.tenant = fields.tenant \
                      AND catalog.entity_type = fields.entity_type \
                      AND catalog.entity_id = fields.entity_id \
               )",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .execute(&mut *tx)
        .await
        .map_err(storage_error)?;

        tx.commit().await.map_err(storage_error)
    }
}
