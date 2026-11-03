//! [`EventStore`] trait implementation for Turso.

use crate::driver::{Value, params};
use temper_runtime::persistence::{
    CommitRows, EntityVectorCandidate, EntityVectorRow, EventMetadata, EventStore,
    PersistenceAppend, PersistenceAppendResult, PersistenceEnvelope, PersistenceError,
    ScheduledActionRecord, pack_f32_le, storage_error, unpack_f32_le,
};
use temper_runtime::tenant::parse_persistence_id_parts;
use tracing::instrument;

use super::TursoEventStore;
use super::append_commit::OwnedAppend;
use super::write_gate::WritePriority;

/// Every live `(entity_type, entity_id)` of a tenant. The `NOT EXISTS` probe is
/// served by `idx_events_deleted`; see `schema::CREATE_EVENTS_DELETED_INDEX`.
pub(super) const LIST_ENTITY_IDS_SQL: &str = concat!(
    "\
SELECT DISTINCT e.entity_type, e.entity_id
    FROM events e
    WHERE e.tenant = ?1
      AND NOT EXISTS (
        SELECT 1
        FROM events d
        WHERE d.tenant = e.tenant
          AND d.entity_type = e.entity_type
          AND d.entity_id = e.entity_id
          ",
    "AND ",
    tombstone_sql!(),
    "
      )"
);

/// At most `?3` live ids of one entity type, in id order.
pub(super) const LIST_ENTITY_IDS_OF_TYPE_LIMITED_SQL: &str = concat!(
    "\
SELECT DISTINCT e.entity_type, e.entity_id
    FROM events e
    WHERE e.tenant = ?1
      AND e.entity_type = ?2
      AND NOT EXISTS (
        SELECT 1
        FROM events d
        WHERE d.tenant = e.tenant
          AND d.entity_type = e.entity_type
          AND d.entity_id = e.entity_id
          ",
    "AND ",
    tombstone_sql!(),
    "
      )
    ORDER BY e.entity_type, e.entity_id
    LIMIT ?3"
);

/// At most `?2` live `(entity_type, entity_id)` pairs of a tenant, in order.
pub(super) const LIST_ENTITY_IDS_LIMITED_SQL: &str = concat!(
    "\
SELECT DISTINCT e.entity_type, e.entity_id
    FROM events e
    WHERE e.tenant = ?1
      AND NOT EXISTS (
        SELECT 1
        FROM events d
        WHERE d.tenant = e.tenant
          AND d.entity_type = e.entity_type
          AND d.entity_id = e.entity_id
          ",
    "AND ",
    tombstone_sql!(),
    "
      )
    ORDER BY e.entity_type, e.entity_id
    LIMIT ?2"
);

impl EventStore for TursoEventStore {
    #[instrument(skip_all, fields(persistence_id, otel.name = "turso.append"))]
    async fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        self.append_owned(OwnedAppend::new(
            persistence_id,
            expected_sequence,
            events,
            CommitRows::default(),
        ))
        .await
    }

    async fn list_scheduled_actions(
        &self,
        tenant: &str,
    ) -> Result<Vec<ScheduledActionRecord>, PersistenceError> {
        self.scheduled_action_rows(tenant).await
    }

    async fn list_entity_scheduled_actions(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<Vec<ScheduledActionRecord>, PersistenceError> {
        self.entity_scheduled_action_rows(tenant, entity_type, entity_id)
            .await
    }

    async fn delete_scheduled_action(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> Result<(), PersistenceError> {
        self.remove_scheduled_action(tenant, entity_type, entity_id, arm_id)
            .await
    }

    async fn lookup_by_key(
        &self,
        tenant: &str,
        entity_type: &str,
        key_name: &str,
        key_hash: &str,
    ) -> Result<Option<String>, PersistenceError> {
        // ADR-0153: a single keyed probe of entity_key_index — present/absent in
        // O(log n), no candidate scan (the negative-existence access path). Bounded
        // regardless of how many entities the tenant/type holds, so it cannot trip
        // the scan budget that produces the 413. It names the key's index: without
        // it the planner may read every row of the type (driver/plan_check.rs).
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT entity_id \
                 FROM entity_key_index INDEXED BY sqlite_autoindex_entity_key_index_1 \
                 WHERE tenant = ?1 AND entity_type = ?2 AND key_name = ?3 AND key_hash = ?4",
                params![tenant, entity_type, key_name, key_hash],
            )
            .await
            .map_err(storage_error)?;
        match rows.next().await.map_err(storage_error)? {
            Some(row) => Ok(Some(row.get::<String>(0).map_err(storage_error)?)),
            None => Ok(None),
        }
    }

    // NOTE (ADR-0153): Turso intentionally does NOT implement `backfill_entity_keys`,
    // `mark_key_index_backfilled`, or `key_index_backfilled_types` — it keeps the
    // no-op/empty trait defaults. Turso never co-commits key rows (it does not override
    // `append_with_keys`), so its `entity_key_index` is never maintained on write. A
    // store that does not maintain the index live must NEVER become authoritative for
    // absence: backfilling or watermarking it would let a keyed miss wrongly read a
    // present entity as absent (or serve a stale keyed hit). Postgres (the current
    // query-plane backend) co-commits and is authoritative; the sim store does too for
    // DST. Giving Turso the keyed oracle requires first implementing live co-commit
    // (completing ADR-0153 phase 2 for Turso) — tracked separately.

    // One transaction per move: the event, its scheduled actions, and the
    // entity's query-plane, timer and vector rows (ADR-0155) commit together,
    // so an event never lands without them and a move pays one write lock.
    // Keys are not maintained on Turso, per the note above.
    async fn append_with_rows(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        rows: CommitRows<'_>,
    ) -> Result<u64, PersistenceError> {
        self.append_owned(OwnedAppend::new(
            persistence_id,
            expected_sequence,
            events,
            rows,
        ))
        .await
    }

    fn commits_entity_rows(&self) -> bool {
        true
    }

    async fn backfill_entity_vectors(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        vector_rows: &[EntityVectorRow],
    ) -> Result<(), PersistenceError> {
        // Reconcile: DELETE all of the entity's rows, then insert the current ones.
        // Empty `vector_rows` purges the entity (deleted / un-embedded). Always runs
        // the delete so a purge is honored.
        let conn = self
            .write_connection("turso.backfill_entity_vectors", WritePriority::Low)
            .await?;
        let tx = conn.begin_immediate().await.map_err(storage_error)?;
        tx.execute(
            // The entity's index (driver/plan_check.rs).
            "DELETE FROM entity_vector_index INDEXED BY idx_evi_entity \
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
            params![tenant, entity_type, entity_id],
        )
        .await
        .map_err(storage_error)?;
        for row in vector_rows {
            tx.execute(
                "INSERT INTO entity_vector_index \
                 (tenant, entity_type, decl_name, model_tag, entity_id, vector, sequence_nr) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
                params![
                    tenant,
                    entity_type,
                    row.decl_name.as_str(),
                    row.model_tag.as_str(),
                    entity_id,
                    Value::Blob(pack_f32_le(&row.vector)),
                ],
            )
            .await
            .map_err(storage_error)?;
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }

    async fn vector_candidates(
        &self,
        tenant: &str,
        entity_type: &str,
        decl_name: &str,
        model_tag: &str,
        limit: usize,
    ) -> Result<Vec<EntityVectorCandidate>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                // The partition's index (driver/plan_check.rs).
                "SELECT entity_id, vector FROM entity_vector_index INDEXED BY idx_evi_partition \
                 WHERE tenant = ?1 AND entity_type = ?2 AND decl_name = ?3 AND model_tag = ?4 \
                 ORDER BY entity_id LIMIT ?5",
                params![tenant, entity_type, decl_name, model_tag, limit as i64],
            )
            .await
            .map_err(storage_error)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let entity_id: String = row.get(0).map_err(storage_error)?;
            let bytes: Vec<u8> = row.get(1).map_err(storage_error)?;
            if let Some(vector) = unpack_f32_le(&bytes) {
                out.push(EntityVectorCandidate { entity_id, vector });
            }
        }
        Ok(out)
    }

    async fn mark_vector_index_backfilled(
        &self,
        tenant: &str,
        entity_type: &str,
        vector_set: &str,
    ) -> Result<(), PersistenceError> {
        let conn = self
            .write_connection("turso.mark_vector_index_backfilled", WritePriority::Low)
            .await?;
        let completed_at = temper_runtime::scheduler::sim_now().to_rfc3339();
        conn.execute(
            "INSERT INTO vector_index_backfill_watermark (tenant, entity_type, vector_set, completed_at) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(tenant, entity_type) \
             DO UPDATE SET vector_set = excluded.vector_set, completed_at = excluded.completed_at",
            params![tenant, entity_type, vector_set, completed_at.as_str()],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    async fn vector_index_backfilled_types(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT entity_type, vector_set FROM vector_index_backfill_watermark \
                 WHERE tenant = ?1",
                params![tenant],
            )
            .await
            .map_err(storage_error)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let entity_type: String = row.get(0).map_err(storage_error)?;
            let vector_set: String = row.get(1).map_err(storage_error)?;
            out.push((entity_type, vector_set));
        }
        Ok(out)
    }

    async fn vectored_entity_ids_for_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT DISTINCT entity_id FROM entity_vector_index \
                 WHERE tenant = ?1 AND entity_type = ?2",
                params![tenant, entity_type],
            )
            .await
            .map_err(storage_error)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(row.get::<String>(0).map_err(storage_error)?);
        }
        Ok(out)
    }

    #[instrument(skip_all, fields(otel.name = "turso.append_batch"))]
    async fn append_batch(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        if appends.is_empty() {
            return Ok(Vec::new());
        }
        if let [append] = appends {
            let sequence_nr = self
                .append_with_rows(
                    &append.persistence_id,
                    append.expected_sequence,
                    &append.events,
                    append.commit_rows(),
                )
                .await?;
            return Ok(vec![PersistenceAppendResult {
                persistence_id: append.persistence_id.clone(),
                sequence_nr,
            }]);
        }

        self.append_batch_owned(appends.to_vec()).await
    }

    #[instrument(skip_all, fields(persistence_id, otel.name = "turso.read_events"))]
    async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let conn = self.configured_connection().await?;

        let mut rows = conn
            .query(
                "SELECT sequence_nr, event_type, payload, metadata
                 FROM events
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND sequence_nr > ?4
                 ORDER BY sequence_nr ASC",
                params![tenant, entity_type, entity_id, from_sequence as i64],
            )
            .await
            .map_err(storage_error)?;

        collect_envelopes(&mut rows).await
    }

    #[instrument(skip_all, fields(persistence_id, otel.name = "turso.read_events_page"))]
    async fn read_events_page(
        &self,
        persistence_id: &str,
        before_sequence: Option<u64>,
        limit: usize,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let conn = self.configured_connection().await?;
        // Both bounds saturate: a sequence or a row count above i64::MAX is
        // past every stored row, so it means "no bound".
        let before = before_sequence.map_or(i64::MAX, |s| i64::try_from(s).unwrap_or(i64::MAX));
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);

        // Newest first along the (tenant, entity_type, entity_id, sequence_nr)
        // unique index, so only the page's rows are read.
        let mut rows = conn
            .query(
                "SELECT sequence_nr, event_type, payload, metadata
                 FROM events
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND sequence_nr < ?4
                 ORDER BY sequence_nr DESC
                 LIMIT ?5",
                params![tenant, entity_type, entity_id, before, limit],
            )
            .await
            .map_err(storage_error)?;

        let mut page = collect_envelopes(&mut rows).await?;
        page.reverse();
        Ok(page)
    }

    async fn first_event_at(
        &self,
        persistence_id: &str,
    ) -> Result<Option<chrono::DateTime<chrono::Utc>>, PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let conn = self.configured_connection().await?;
        // One read keeps the saved fact and the journal fallback consistent
        // with compaction. Both probes use the full record key.
        let mut rows = conn.query(
            "SELECT COALESCE(
                (SELECT first_event_at FROM entity_catalog INDEXED BY sqlite_autoindex_entity_catalog_1
                 WHERE tenant=?1 AND entity_type=?2 AND entity_id=?3),
                (SELECT COALESCE(json_extract(payload, '$.timestamp'), json_extract(metadata, '$.timestamp'))
                 FROM events INDEXED BY idx_events_entity
                 WHERE tenant=?1 AND entity_type=?2 AND entity_id=?3 AND sequence_nr=1))",
            params![tenant, entity_type, entity_id]).await.map_err(storage_error)?;
        let row = rows
            .next()
            .await
            .map_err(storage_error)?
            .ok_or_else(|| PersistenceError::Storage("missing creation lookup row".into()))?;
        let saved: Option<String> = row.get(0).map_err(storage_error)?;
        saved
            .map(|saved| {
                chrono::DateTime::parse_from_rfc3339(&saved)
                    .map(|t| t.with_timezone(&chrono::Utc))
                    .map_err(|error| PersistenceError::Serialization(error.to_string()))
            })
            .transpose()
    }

    #[instrument(skip_all, fields(persistence_id, otel.name = "turso.save_snapshot"))]
    async fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let conn = self
            .write_connection("turso.save_snapshot", WritePriority::Low)
            .await?;
        let tx = conn.begin_immediate().await.map_err(storage_error)?;

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

        tx.execute("INSERT INTO entity_projection_delta (tenant, entity_type, entity_id, sequence_nr)
            SELECT ?1, ?2, ?3, ?4 WHERE NOT EXISTS (
                SELECT 1 FROM entity_catalog c INDEXED BY idx_entity_catalog_checkpoint LEFT JOIN entity_catalog_counters k
                    ON k.tenant = c.tenant AND k.entity_type = c.entity_type AND k.entity_id = c.entity_id
                WHERE c.tenant = ?1 AND c.entity_type = ?2 AND c.entity_id = ?3
                    AND COALESCE(k.sequence_nr, c.sequence_nr) >= ?4)
            ON CONFLICT (tenant, entity_type, entity_id) DO UPDATE SET sequence_nr = MAX(sequence_nr, excluded.sequence_nr)",
            params![tenant, entity_type, entity_id, sequence_nr as i64]).await.map_err(storage_error)?;

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

    #[instrument(skip_all, fields(persistence_id, otel.name = "turso.load_snapshot"))]
    async fn load_snapshot(
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

    async fn latest_sequence_nr(&self, persistence_id: &str) -> Result<u64, PersistenceError> {
        let (tenant, ty, id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        self.journal_sequence_nr(tenant, ty, id).await
    }

    async fn journal_heads(
        &self,
        tenant: &str,
    ) -> Result<Vec<temper_runtime::persistence::JournalHead>, PersistenceError> {
        self.load_journal_heads(tenant).await
    }

    #[instrument(skip_all, fields(tenant, otel.name = "turso.list_entity_ids"))]
    async fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(LIST_ENTITY_IDS_SQL, params![tenant])
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let entity_type = row.get::<String>(0).map_err(storage_error)?;
            let entity_id = row.get::<String>(1).map_err(storage_error)?;
            out.push((entity_type, entity_id));
        }
        Ok(out)
    }

    async fn list_entity_ids_by_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        self.list_entity_ids_by_type_from_read_sources(tenant, entity_type)
            .await
    }

    async fn list_entity_ids_limited(
        &self,
        tenant: &str,
        entity_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.configured_connection().await?;
        let limit = limit.min(i64::MAX as usize) as i64;
        let mut out = Vec::new();

        if let Some(entity_type) = entity_type {
            let mut rows = conn
                .query(
                    LIST_ENTITY_IDS_OF_TYPE_LIMITED_SQL,
                    params![tenant, entity_type, limit],
                )
                .await
                .map_err(storage_error)?;

            while let Some(row) = rows.next().await.map_err(storage_error)? {
                out.push((
                    row.get::<String>(0).map_err(storage_error)?,
                    row.get::<String>(1).map_err(storage_error)?,
                ));
            }
            return Ok(out);
        }

        let mut rows = conn
            .query(LIST_ENTITY_IDS_LIMITED_SQL, params![tenant, limit])
            .await
            .map_err(storage_error)?;

        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push((
                row.get::<String>(0).map_err(storage_error)?,
                row.get::<String>(1).map_err(storage_error)?,
            ));
        }
        Ok(out)
    }
}

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
}

/// Decode `SELECT sequence_nr, event_type, payload, metadata` rows into
/// envelopes, in the order the query returned them.
async fn collect_envelopes(
    rows: &mut crate::driver::Rows,
) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.map_err(storage_error)? {
        let seq = row.get::<i64>(0).map_err(storage_error)? as u64;
        let event_type = row.get::<String>(1).map_err(storage_error)?;
        let payload_json = row.get::<String>(2).map_err(storage_error)?;
        let metadata_json = row.get::<Option<String>>(3).map_err(storage_error)?;

        let payload = serde_json::from_str(&payload_json).map_err(|e| {
            tracing::error!(error = %e, "failed to deserialize event payload");
            PersistenceError::Serialization(e.to_string())
        })?;
        let metadata_raw = metadata_json.ok_or_else(|| {
            tracing::error!("missing event metadata");
            PersistenceError::Serialization("missing event metadata".to_string())
        })?;
        let metadata: EventMetadata = serde_json::from_str(&metadata_raw).map_err(|e| {
            tracing::error!(error = %e, "failed to deserialize event metadata");
            PersistenceError::Serialization(e.to_string())
        })?;

        out.push(PersistenceEnvelope {
            sequence_nr: seq,
            event_type,
            payload,
            metadata,
        });
    }
    Ok(out)
}
