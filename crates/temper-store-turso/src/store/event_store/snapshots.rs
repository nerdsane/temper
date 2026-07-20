//! Snapshot persistence operations.

use super::*;

impl TursoEventStore {
    #[instrument(skip_all, fields(persistence_id, otel.name = "turso.save_snapshot"))]
    pub(super) async fn save_snapshot_impl(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let _write_permit = self
            .acquire_write_permit("turso.save_snapshot", WritePriority::Low)
            .await?;
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;

        tx.execute(
            "INSERT INTO snapshots (tenant, entity_type, entity_id, sequence_nr, snapshot)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (tenant, entity_type, entity_id)
             DO UPDATE SET
                sequence_nr = excluded.sequence_nr,
                snapshot = excluded.snapshot,
                created_at = datetime('now')",
            params![
                tenant,
                entity_type,
                entity_id,
                sequence_nr as i64,
                snapshot.to_vec()
            ],
        )
        .await
        .map_err(storage_error)?;

        tx.execute(
            "INSERT INTO snapshot_history (tenant, entity_type, entity_id, sequence_nr, snapshot)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (tenant, entity_type, entity_id, sequence_nr)
             DO UPDATE SET snapshot = excluded.snapshot, created_at = datetime('now')",
            params![
                tenant,
                entity_type,
                entity_id,
                sequence_nr as i64,
                snapshot.to_vec()
            ],
        )
        .await
        .map_err(storage_error)?;

        let mut segment_rows = tx
            .query(
                "SELECT COALESCE(MAX(segment_index), 0)
                 FROM events
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND sequence_nr <= ?4",
                params![tenant, entity_type, entity_id, sequence_nr as i64],
            )
            .await
            .map_err(storage_error)?;
        let current_segment = match segment_rows.next().await.map_err(storage_error)? {
            Some(row) => row.get::<i64>(0).map_err(storage_error)?,
            None => 0,
        };
        drop(segment_rows);

        tx.execute(
            "INSERT INTO event_segments
             (tenant, entity_type, entity_id, segment_index, start_sequence_nr, end_sequence_nr, snapshot_sequence, event_count, sealed_at)
             VALUES (?1, ?2, ?3, ?4, 1, ?5, ?5, ?5, datetime('now'))
             ON CONFLICT(tenant, entity_type, entity_id, segment_index) DO NOTHING",
            params![
                tenant,
                entity_type,
                entity_id,
                current_segment,
                sequence_nr as i64
            ],
        )
        .await
        .map_err(storage_error)?;

        tx.execute(
            "UPDATE event_segments
             SET end_sequence_nr = ?5,
                 snapshot_sequence = ?5,
                 sealed_at = datetime('now'),
                 event_count = MAX(?5 - start_sequence_nr + 1, 0)
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND segment_index = ?4",
            params![
                tenant,
                entity_type,
                entity_id,
                current_segment,
                sequence_nr as i64
            ],
        )
        .await
        .map_err(storage_error)?;

        tx.execute(
            "INSERT INTO event_segments
             (tenant, entity_type, entity_id, segment_index, start_sequence_nr)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(tenant, entity_type, entity_id, segment_index) DO NOTHING",
            params![
                tenant,
                entity_type,
                entity_id,
                current_segment + 1,
                sequence_nr as i64 + 1
            ],
        )
        .await
        .map_err(storage_error)?;

        tx.commit().await.map_err(storage_error)?;

        Ok(())
    }

    #[instrument(skip_all, fields(persistence_id, otel.name = "turso.replace_snapshot"))]
    pub(super) async fn replace_snapshot_impl(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        expected_snapshot: &[u8],
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let _write_permit = self
            .acquire_write_permit("turso.replace_snapshot", WritePriority::High)
            .await?;
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;

        let updated = tx
            .execute(
                "UPDATE snapshots SET snapshot = ?5, created_at = datetime('now')
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3
                   AND sequence_nr = ?4 AND snapshot = ?6",
                params![
                    tenant,
                    entity_type,
                    entity_id,
                    sequence_nr as i64,
                    snapshot.to_vec(),
                    expected_snapshot.to_vec()
                ],
            )
            .await
            .map_err(storage_error)?;
        if updated != 1 {
            return Err(PersistenceError::Storage(format!(
                "snapshot changed or is missing at sequence {sequence_nr} for {persistence_id}"
            )));
        }

        tx.execute(
            "INSERT INTO snapshot_history (tenant, entity_type, entity_id, sequence_nr, snapshot)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (tenant, entity_type, entity_id, sequence_nr)
             DO UPDATE SET snapshot = excluded.snapshot, created_at = datetime('now')",
            params![
                tenant,
                entity_type,
                entity_id,
                sequence_nr as i64,
                snapshot.to_vec()
            ],
        )
        .await
        .map_err(storage_error)?;

        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }

    #[instrument(skip_all, fields(persistence_id, otel.name = "turso.load_snapshot"))]
    pub(super) async fn load_snapshot_impl(
        &self,
        persistence_id: &str,
    ) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT sequence_nr, snapshot
                 FROM snapshots
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3
                 ORDER BY sequence_nr DESC
                 LIMIT 1",
                params![tenant, entity_type, entity_id],
            )
            .await
            .map_err(storage_error)?;

        let Some(row) = rows.next().await.map_err(storage_error)? else {
            return Ok(None);
        };

        let sequence_nr = row.get::<i64>(0).map_err(storage_error)? as u64;
        let snapshot = row.get::<Vec<u8>>(1).map_err(storage_error)?;
        Ok(Some((sequence_nr, snapshot)))
    }
}
