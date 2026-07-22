//! Conditional query-projection removal and dirty repair.

use super::*;

impl TursoEventStore {
    /// Remove the durable query-plane projection for a single entity.
    #[instrument(skip_all, fields(
        otel.name = "turso.remove_query_projection",
        tenant, entity_type, entity_id,
    ))]
    pub async fn remove_query_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), PersistenceError> {
        self.remove_query_projection_inner(tenant, entity_type, entity_id, None)
            .await
            .map(|_| ())
    }

    /// Remove a projection only while its exact journal/snapshot source is current.
    pub async fn remove_query_projection_if_source(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        source: ProjectionSourceFence<'_>,
    ) -> Result<bool, PersistenceError> {
        self.remove_query_projection_inner(tenant, entity_type, entity_id, Some(source))
            .await
    }

    /// Clear a dirty marker only while its exact journal/snapshot source is current.
    pub async fn clear_query_projection_dirty_if_source(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        source: ProjectionSourceFence<'_>,
    ) -> Result<bool, PersistenceError> {
        let _write_permit = self
            .acquire_write_permit(
                "turso.clear_query_projection_dirty_if_source",
                WritePriority::Low,
            )
            .await?;
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;
        let journal_sequence = {
            let mut rows = tx
                .query(
                    "SELECT COALESCE(MAX(sequence_nr), 0) FROM events \
                     WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                    params![tenant, entity_type, entity_id],
                )
                .await
                .map_err(storage_error)?;
            rows.next()
                .await
                .map_err(storage_error)?
                .map(|row| row.get::<i64>(0).map_err(storage_error))
                .transpose()?
                .unwrap_or(0)
        };
        let current_snapshot = {
            let mut rows = tx
                .query(
                    "SELECT sequence_nr, snapshot FROM snapshots \
                     WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                    params![tenant, entity_type, entity_id],
                )
                .await
                .map_err(storage_error)?;
            rows.next()
                .await
                .map_err(storage_error)?
                .map(|row| {
                    Ok::<_, PersistenceError>((
                        row.get::<i64>(0).map_err(storage_error)?,
                        row.get::<Vec<u8>>(1).map_err(storage_error)?,
                    ))
                })
                .transpose()?
        };
        let journal_matches =
            u64::try_from(journal_sequence).ok() == Some(source.expected_journal_sequence);
        let snapshot_matches = match (source.expected_snapshot, current_snapshot.as_ref()) {
            (None, None) => true,
            (Some(expected), Some((sequence_nr, snapshot))) => {
                u64::try_from(*sequence_nr).ok() == Some(expected.sequence_nr)
                    && snapshot.as_slice() == expected.state
            }
            _ => false,
        };
        if !journal_matches || !snapshot_matches {
            tx.commit().await.map_err(storage_error)?;
            return Ok(false);
        }
        clear_query_projection_dirty(&tx, tenant, entity_type, entity_id).await?;
        tx.commit().await.map_err(storage_error)?;
        Ok(true)
    }

    async fn remove_query_projection_inner(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        source: Option<ProjectionSourceFence<'_>>,
    ) -> Result<bool, PersistenceError> {
        let source_fenced = source.is_some();
        let _write_permit = self
            .acquire_write_permit("turso.remove_query_projection", WritePriority::Low)
            .await?;
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;
        let journal_sequence = {
            let mut journal_rows = tx
                .query(
                    "SELECT COALESCE(MAX(sequence_nr), 0) FROM events \
                     WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                    params![tenant, entity_type, entity_id],
                )
                .await
                .map_err(storage_error)?;
            journal_rows
                .next()
                .await
                .map_err(storage_error)?
                .map(|row| row.get::<i64>(0).map_err(storage_error))
                .transpose()?
                .unwrap_or(0)
        };
        let current_snapshot = {
            let mut snapshot_rows = tx
                .query(
                    "SELECT sequence_nr, snapshot FROM snapshots \
                     WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                    params![tenant, entity_type, entity_id],
                )
                .await
                .map_err(storage_error)?;
            snapshot_rows
                .next()
                .await
                .map_err(storage_error)?
                .map(|row| {
                    Ok::<_, PersistenceError>((
                        row.get::<i64>(0).map_err(storage_error)?,
                        row.get::<Vec<u8>>(1).map_err(storage_error)?,
                    ))
                })
                .transpose()?
        };
        let source_backed = journal_sequence > 0 || current_snapshot.is_some();
        if let Some(source) = source {
            let journal_matches =
                u64::try_from(journal_sequence).ok() == Some(source.expected_journal_sequence);
            let snapshot_matches = match (source.expected_snapshot, current_snapshot.as_ref()) {
                (None, None) => true,
                (Some(expected), Some((sequence_nr, snapshot))) => {
                    u64::try_from(*sequence_nr).ok() == Some(expected.sequence_nr)
                        && snapshot.as_slice() == expected.state
                }
                _ => false,
            };
            if !journal_matches || !snapshot_matches {
                tx.commit().await.map_err(storage_error)?;
                return Ok(false);
            }
        }
        tx.execute(
            "DELETE FROM entity_catalog WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
            params![tenant, entity_type, entity_id],
        )
        .await
        .map_err(storage_error)?;
        tx.execute(
            "DELETE FROM entity_field_index WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
            params![tenant, entity_type, entity_id],
        )
        .await
        .map_err(storage_error)?;
        if source_fenced {
            clear_query_projection_dirty(&tx, tenant, entity_type, entity_id).await?;
        } else if source_backed {
            mark_query_projection_dirty(&tx, tenant, entity_type, entity_id).await?;
        } else {
            clear_query_projection_dirty(&tx, tenant, entity_type, entity_id).await?;
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(true)
    }

    /// Remove only an exact attempted projection row during unstable-source cleanup.
    #[expect(
        clippy::too_many_arguments,
        reason = "exact projection cleanup boundary"
    )]
    pub async fn remove_query_projection_if_exact(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        state: &serde_json::Value,
        sequence_nr: u64,
    ) -> Result<bool, PersistenceError> {
        let _write_permit = self
            .acquire_write_permit("turso.remove_query_projection_if_exact", WritePriority::Low)
            .await?;
        let status = canonical_projection_status(status, state);
        let fields_json = serde_json::to_string(fields)
            .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
        let state_json = serde_json::to_string(state)
            .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
        let sequence_nr = i64::try_from(sequence_nr)
            .map_err(|_| PersistenceError::Storage("projection sequence exceeds i64".into()))?;
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;
        let removed = tx
            .execute(
                "DELETE FROM entity_catalog \
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 \
                   AND status = ?4 AND fields = ?5 AND state = ?6 AND sequence_nr = ?7",
                params![
                    tenant,
                    entity_type,
                    entity_id,
                    status,
                    fields_json.as_str(),
                    state_json.as_str(),
                    sequence_nr,
                ],
            )
            .await
            .map_err(storage_error)?;
        if removed > 0 {
            tx.execute(
                "DELETE FROM entity_field_index \
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                params![tenant, entity_type, entity_id],
            )
            .await
            .map_err(storage_error)?;
            mark_query_projection_dirty(&tx, tenant, entity_type, entity_id).await?;
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(removed > 0)
    }

    /// Backwards-compatible alias for the old name.
    #[instrument(skip_all, fields(
        otel.name = "turso.remove_field_index",
        tenant, entity_type, entity_id,
    ))]
    pub async fn remove_field_index(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), PersistenceError> {
        self.remove_query_projection(tenant, entity_type, entity_id)
            .await
    }
}
