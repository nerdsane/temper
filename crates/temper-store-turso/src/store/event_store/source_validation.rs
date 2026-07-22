//! Snapshot-source validation and event-segment selection.

use super::*;

impl TursoEventStore {
    /// List tenants with at least one persisted event.
    #[instrument(skip_all, fields(otel.name = "turso.list_event_tenants"))]
    pub async fn list_event_tenants(&self) -> Result<Vec<String>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query("SELECT DISTINCT tenant FROM events ORDER BY tenant", ())
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(row.get::<String>(0).map_err(storage_error)?);
        }
        Ok(out)
    }

    /// List tenants appearing in any tenant-scoped storage table.
    #[instrument(skip_all, fields(otel.name = "turso.list_storage_tenants"))]
    pub async fn list_storage_tenants(&self) -> Result<Vec<String>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT tenant FROM events \
                 UNION SELECT tenant FROM event_segments \
                 UNION SELECT tenant FROM snapshot_history \
                 UNION SELECT tenant FROM specs \
                 UNION SELECT tenant FROM trajectories \
                 UNION SELECT tenant FROM tenant_constraints \
                 UNION SELECT tenant FROM wasm_modules \
                 UNION SELECT tenant FROM wasm_invocation_logs \
                 UNION SELECT tenant FROM pending_decisions \
                 UNION SELECT tenant FROM tenant_policies \
                 UNION SELECT tenant FROM policies \
                 UNION SELECT tenant_id AS tenant FROM tenant_installed_apps \
                 UNION SELECT tenant FROM policy_denial_patterns \
                 UNION SELECT tenant FROM tenant_secrets \
                 UNION SELECT tenant FROM design_time_events \
                 UNION SELECT tenant FROM ots_trajectories \
                 UNION SELECT tenant FROM entity_catalog \
                 ORDER BY tenant",
                (),
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let tenant = row.get::<String>(0).map_err(storage_error)?;
            if !tenant.trim().is_empty() {
                out.push(tenant);
            }
        }
        Ok(out)
    }

    pub(super) async fn validate_snapshot_source(
        tx: &libsql::Transaction,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        source: &SnapshotSourceFence,
    ) -> Result<(), PersistenceError> {
        if matches!(source, SnapshotSourceFence::Unchecked) {
            return Ok(());
        }
        let mut rows = tx
            .query(
                "SELECT sequence_nr, snapshot FROM snapshots \
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                params![tenant, entity_type, entity_id],
            )
            .await
            .map_err(storage_error)?;
        let current = match rows.next().await.map_err(storage_error)? {
            Some(row) => Some((
                row.get::<i64>(0).map_err(storage_error)?,
                row.get::<Vec<u8>>(1).map_err(storage_error)?,
            )),
            None => None,
        };
        let matches = match source {
            SnapshotSourceFence::Unchecked => true,
            SnapshotSourceFence::Absent => current.is_none(),
            SnapshotSourceFence::Exact { sequence_nr, state } => current.is_some_and(|current| {
                u64::try_from(current.0).ok() == Some(*sequence_nr)
                    && current.1.as_slice() == state.as_slice()
            }),
        };
        if matches {
            Ok(())
        } else {
            Err(PersistenceError::SnapshotGenerationChanged)
        }
    }

    /// Return the open segment that must receive the next journal event.
    /// Repairs legacy snapshot-only and future-boundary topology in the same
    /// transaction that performs the append.
    pub(super) async fn prepare_open_segment(
        tx: &libsql::Transaction,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        current_sequence: u64,
    ) -> Result<i64, PersistenceError> {
        let current_sequence = i64::try_from(current_sequence).map_err(|_| {
            PersistenceError::Storage("journal sequence exceeds SQLite integer".to_string())
        })?;
        let next_sequence = current_sequence.checked_add(1).ok_or_else(|| {
            PersistenceError::Storage("journal sequence exceeds SQLite integer".to_string())
        })?;

        if current_sequence == 0 {
            tx.execute(
                "DELETE FROM event_segments \
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                params![tenant, entity_type, entity_id],
            )
            .await
            .map_err(storage_error)?;
            tx.execute(
                "INSERT INTO event_segments \
                 (tenant, entity_type, entity_id, segment_index, start_sequence_nr, \
                  end_sequence_nr, snapshot_sequence, event_count, sealed_at) \
                 VALUES (?1, ?2, ?3, 0, 1, NULL, NULL, 0, NULL)",
                params![tenant, entity_type, entity_id],
            )
            .await
            .map_err(storage_error)?;
            return Ok(0);
        }

        let mut open_rows = tx
            .query(
                "SELECT segment_index FROM event_segments \
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 \
                   AND sealed_at IS NULL AND start_sequence_nr <= ?4 \
                 ORDER BY segment_index DESC LIMIT 1",
                params![tenant, entity_type, entity_id, next_sequence],
            )
            .await
            .map_err(storage_error)?;
        if let Some(row) = open_rows.next().await.map_err(storage_error)? {
            return row.get::<i64>(0).map_err(storage_error);
        }
        drop(open_rows);

        let mut event_rows = tx
            .query(
                "SELECT segment_index FROM events \
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 \
                 ORDER BY sequence_nr DESC LIMIT 1",
                params![tenant, entity_type, entity_id],
            )
            .await
            .map_err(storage_error)?;
        let last_event_segment = match event_rows.next().await.map_err(storage_error)? {
            Some(row) => row.get::<i64>(0).map_err(storage_error)?,
            None => 0,
        };
        drop(event_rows);

        let mut segment_rows = tx
            .query(
                "SELECT start_sequence_nr, snapshot_sequence, sealed_at IS NOT NULL \
                 FROM event_segments \
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 \
                   AND segment_index = ?4",
                params![tenant, entity_type, entity_id, last_event_segment],
            )
            .await
            .map_err(storage_error)?;
        let segment = match segment_rows.next().await.map_err(storage_error)? {
            Some(row) => Some((
                row.get::<i64>(0).map_err(storage_error)?,
                row.get::<Option<i64>>(1).map_err(storage_error)?,
                row.get::<i64>(2).map_err(storage_error)? != 0,
            )),
            None => None,
        };
        drop(segment_rows);

        let stale_future_boundary = segment.as_ref().is_some_and(|(_, snapshot, sealed)| {
            *sealed && snapshot.is_some_and(|snapshot| snapshot > current_sequence)
        });
        if segment.as_ref().is_some_and(|(_, _, sealed)| !sealed) || stale_future_boundary {
            let start = segment.map_or(1, |(start, _, _)| start);
            let count = (current_sequence - start + 1).max(0);
            tx.execute(
                "DELETE FROM event_segments \
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 \
                   AND segment_index > ?4",
                params![tenant, entity_type, entity_id, last_event_segment],
            )
            .await
            .map_err(storage_error)?;
            tx.execute(
                "INSERT INTO event_segments \
                 (tenant, entity_type, entity_id, segment_index, start_sequence_nr, \
                  end_sequence_nr, snapshot_sequence, event_count, sealed_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, NULL) \
                 ON CONFLICT(tenant, entity_type, entity_id, segment_index) DO UPDATE SET \
                    start_sequence_nr = excluded.start_sequence_nr, \
                    end_sequence_nr = excluded.end_sequence_nr, \
                    snapshot_sequence = NULL, event_count = excluded.event_count, sealed_at = NULL",
                params![
                    tenant,
                    entity_type,
                    entity_id,
                    last_event_segment,
                    start,
                    current_sequence,
                    count
                ],
            )
            .await
            .map_err(storage_error)?;
            return Ok(last_event_segment);
        }

        let mut max_rows = tx
            .query(
                "SELECT MAX(segment_index) FROM event_segments \
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                params![tenant, entity_type, entity_id],
            )
            .await
            .map_err(storage_error)?;
        let max_segment = match max_rows.next().await.map_err(storage_error)? {
            Some(row) => row
                .get::<Option<i64>>(0)
                .map_err(storage_error)?
                .unwrap_or(-1),
            None => -1,
        };
        let segment_index = max_segment.checked_add(1).ok_or_else(|| {
            PersistenceError::Storage("event segment index exceeds SQLite integer".to_string())
        })?;
        tx.execute(
            "INSERT INTO event_segments \
             (tenant, entity_type, entity_id, segment_index, start_sequence_nr) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![tenant, entity_type, entity_id, segment_index, next_sequence],
        )
        .await
        .map_err(storage_error)?;
        Ok(segment_index)
    }
}
