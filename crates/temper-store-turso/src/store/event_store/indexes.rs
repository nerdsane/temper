//! Key and vector index event-store operations.

use super::*;

impl TursoEventStore {
    pub(super) async fn lookup_by_key_impl(
        &self,
        tenant: &str,
        entity_type: &str,
        key_name: &str,
        key_hash: &str,
    ) -> Result<Option<String>, PersistenceError> {
        // ADR-0153: a single keyed probe of entity_key_index — present/absent in
        // O(log n), no candidate scan (the negative-existence access path). Bounded
        // regardless of how many entities the tenant/type holds, so it cannot trip
        // the scan budget that produces the 413.
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT entity_id FROM entity_key_index \
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

    // ADR-0155: Turso maintains `entity_vector_index` **write-behind** — the event is
    // appended first (with retries), then the derived vector rows follow in a separate,
    // also-retried write. This is safe for vectors (unlike keys) because a vector row
    // carries no uniqueness constraint and a lagging index write only makes a ranking
    // temporarily incomplete; it can never corrupt a keyed absence. So Turso implements
    // the full vector surface below.

    pub(super) async fn append_with_index_rows_impl(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        _key_rows: &[temper_runtime::persistence::EntityKeyRow],
        vector_rows: &[EntityVectorRow],
        reconcile_vectors: bool,
    ) -> Result<u64, PersistenceError> {
        // The journal append is the durable event (keys are not maintained on Turso,
        // per the note above).
        let new_seq = self
            .append(persistence_id, expected_sequence, events)
            .await?;
        // Write-behind vector maintenance: reconcile the entity's rows (delete stale,
        // insert current — an empty `vector_rows` purges a deleted/cleared entity),
        // RETRIED like the event append rather than a warn-once one-shot, so a
        // transient failure does not silently drop the write. On final exhaustion the
        // error is logged loudly; the partition then lags until the next backfill
        // reconcile runs. Only runs when the type declares vector paths.
        if reconcile_vectors
            && let Ok((tenant, entity_type, entity_id)) = parse_persistence_id_parts(persistence_id)
        {
            let total_attempts = append_max_attempts();
            let mut last_err: Option<PersistenceError> = None;
            for attempt in 0..total_attempts {
                if attempt > 0 {
                    tokio::time::sleep(Duration::from_millis(retry_delay_ms(attempt - 1))).await;
                }
                match self
                    .backfill_entity_vectors(tenant, entity_type, entity_id, vector_rows)
                    .await
                {
                    Ok(()) => {
                        last_err = None;
                        break;
                    }
                    Err(err) => {
                        let transient = matches!(&err, PersistenceError::Storage(msg) if is_transient_write_error(msg));
                        last_err = Some(err);
                        if !transient {
                            break;
                        }
                    }
                }
            }
            if let Some(error) = last_err {
                error!(
                    persistence_id,
                    error = %error,
                    "turso vector-index write-behind failed after retries; partition lags until the next backfill reconcile"
                );
            }
        }
        Ok(new_seq)
    }

    pub(super) async fn backfill_entity_vectors_impl(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        vector_rows: &[EntityVectorRow],
    ) -> Result<(), PersistenceError> {
        // Reconcile: DELETE all of the entity's rows, then insert the current ones.
        // Empty `vector_rows` purges the entity (deleted / un-embedded). Always runs
        // the delete so a purge is honored.
        let _write_permit = self
            .acquire_write_permit("turso.backfill_entity_vectors", WritePriority::Low)
            .await?;
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;
        tx.execute(
            "DELETE FROM entity_vector_index \
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

    pub(super) async fn vector_candidates_impl(
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
                "SELECT entity_id, vector FROM entity_vector_index \
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

    pub(super) async fn mark_vector_index_backfilled_impl(
        &self,
        tenant: &str,
        entity_type: &str,
        vector_set: &str,
    ) -> Result<(), PersistenceError> {
        let _write_permit = self
            .acquire_write_permit("turso.mark_vector_index_backfilled", WritePriority::Low)
            .await?;
        let conn = self.configured_connection().await?;
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

    pub(super) async fn vector_index_backfilled_types_impl(
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

    pub(super) async fn vectored_entity_ids_for_type_impl(
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
}
