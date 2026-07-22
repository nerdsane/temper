//! Atomic multi-stream PostgreSQL journal appends.

use super::*;

impl PostgresEventStore {
    /// Atomically append to multiple entity journals in one PostgreSQL
    /// transaction. Used as the storage foundation for cross-actor Composite
    /// transactions: every stream's optimistic-concurrency and final declared-key
    /// set must validate before any event is inserted.
    pub(super) async fn append_batch_inner(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        if appends.is_empty() {
            return Ok(Vec::new());
        }

        let mut lock_keys = std::collections::BTreeSet::new();
        let mut mutation_types = std::collections::BTreeSet::new();
        let mut unreconciled_types = std::collections::BTreeSet::new();
        let mut type_contracts = std::collections::BTreeMap::new();
        let mut batch_claim = None;
        for append in appends {
            if !append.reconcile_keys && !append.key_rows.is_empty() {
                return Err(PersistenceError::Storage(format!(
                    "append_batch stream '{}' supplied key rows without exact reconciliation",
                    append.persistence_id
                )));
            }
            let (tenant, entity_type, entity_id) =
                parse_persistence_id_parts(&append.persistence_id)
                    .map_err(PersistenceError::Storage)?;
            if append.reconcile_keys || !append.events.is_empty() {
                mutation_types.insert((tenant.to_string(), entity_type.to_string()));
            }
            if !append.events.is_empty() && !append.reconcile_keys {
                unreconciled_types.insert((tenant.to_string(), entity_type.to_string()));
            }
            if append.reconcile_keys {
                let type_key = (tenant.to_string(), entity_type.to_string());
                if let Some(existing) = type_contracts.get(&type_key) {
                    if existing != &append.key_set_signature {
                        return Err(PersistenceError::Storage(format!(
                            "append_batch supplied inconsistent key contracts for {tenant}:{entity_type}"
                        )));
                    }
                } else {
                    type_contracts.insert(type_key, append.key_set_signature.clone());
                }
            }
            let lock_key = event_stream_lock_key(tenant, entity_type, entity_id);
            if !lock_keys.insert(lock_key) {
                return Err(PersistenceError::Storage(format!(
                    "duplicate persistence stream '{}' in append_batch",
                    append.persistence_id
                )));
            }
            if let Some(claim) = &append.batch_idempotency
                && batch_claim.replace(claim).is_some()
            {
                return Err(PersistenceError::Storage(
                    "append_batch supplied more than one idempotency claim".to_string(),
                ));
            }
            if let Some(claim) = &append.batch_idempotency {
                let (claim_tenant, claim_entity_type, claim_entity_id) =
                    parse_persistence_id_parts(&claim.persistence_id)
                        .map_err(PersistenceError::Storage)?;
                lock_keys.insert(event_stream_lock_key(
                    claim_tenant,
                    claim_entity_type,
                    claim_entity_id,
                ));
            }
        }

        let mut transaction_timer = PostgresTransactionTimer::start(EVENT_APPEND_OPERATION);
        let acquire_started = Instant::now();
        let mut conn = match self.pool.acquire().await {
            Ok(conn) => {
                record_postgres_pool_acquire_duration(
                    acquire_started.elapsed(),
                    EVENT_APPEND_OPERATION,
                    "ok",
                );
                conn
            }
            Err(e) => {
                record_postgres_pool_acquire_duration(
                    acquire_started.elapsed(),
                    EVENT_APPEND_OPERATION,
                    "error",
                );
                return Err(PersistenceError::Storage(e.to_string()));
            }
        };
        let begin_started = Instant::now();
        let mut tx = match conn.begin().await {
            Ok(tx) => {
                record_postgres_transaction_begin_duration(
                    begin_started.elapsed(),
                    EVENT_APPEND_OPERATION,
                    "ok",
                );
                tx
            }
            Err(e) => {
                record_postgres_transaction_begin_duration(
                    begin_started.elapsed(),
                    EVENT_APPEND_OPERATION,
                    "error",
                );
                return Err(PersistenceError::Storage(e.to_string()));
            }
        };

        for (tenant, entity_type) in &mutation_types {
            lock_key_contract(&mut tx, tenant, entity_type).await?;
        }
        for lock_key in &lock_keys {
            lock_event_stream(&mut tx, lock_key).await?;
        }

        if let Some(claim) = batch_claim {
            let existing: Option<(String,)> = crate::dbm::postgres_query_as!(
                "SELECT intent_hash FROM persistence_batch_idempotency \
                 WHERE persistence_id = $1 AND idempotency_key = $2",
            )
            .bind(&claim.persistence_id)
            .bind(&claim.idempotency_key)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| PersistenceError::Storage(error.to_string()))?;
            if let Some((committed_hash,)) = existing {
                if committed_hash != claim.intent_hash {
                    return Err(PersistenceError::Storage(format!(
                        "atomic batch idempotency key '{}' was reused with a different intent",
                        claim.idempotency_key
                    )));
                }
                let mut results = Vec::with_capacity(appends.len());
                for append in appends {
                    let (tenant, entity_type, entity_id) =
                        parse_persistence_id_parts(&append.persistence_id)
                            .map_err(PersistenceError::Storage)?;
                    let row: Option<(i64,)> = crate::dbm::postgres_query_as!(
                        "SELECT COALESCE(MAX(sequence_nr), 0) FROM events \
                         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
                    )
                    .bind(tenant)
                    .bind(entity_type)
                    .bind(entity_id)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|error| PersistenceError::Storage(error.to_string()))?;
                    results.push(PersistenceAppendResult {
                        persistence_id: append.persistence_id.clone(),
                        sequence_nr: row.map(|value| value.0 as u64).unwrap_or(0),
                        batch_already_applied: true,
                    });
                }
                transaction_timer.set_outcome("idempotent_replay");
                return Ok(results);
            }
        }

        let mut parsed = Vec::with_capacity(appends.len());
        for append in appends {
            let (tenant, entity_type, entity_id) =
                parse_persistence_id_parts(&append.persistence_id)
                    .map_err(PersistenceError::Storage)?;
            let row: Option<(i64,)> = crate::dbm::postgres_query_as!(
                "SELECT COALESCE(MAX(sequence_nr), 0) FROM events \
                 WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;

            let current_seq = row.map(|r| r.0 as u64).unwrap_or(0);
            if current_seq != append.expected_sequence {
                transaction_timer.set_outcome("concurrency_violation");
                return Err(PersistenceError::ConcurrencyViolation {
                    expected: append.expected_sequence,
                    actual: current_seq,
                });
            }
            if !matches!(&append.snapshot_source, SnapshotSourceFence::Unchecked) {
                let current_snapshot =
                    load_snapshot_for_update(&mut tx, tenant, entity_type, entity_id).await?;
                if !snapshot_source_matches(current_snapshot.as_ref(), &append.snapshot_source) {
                    return Err(PersistenceError::SnapshotGenerationChanged);
                }
            }
            let segment_index = segments::open_segment_for_append(
                &mut tx,
                tenant,
                entity_type,
                entity_id,
                current_seq,
            )
            .await?;
            parsed.push((
                tenant.to_string(),
                entity_type.to_string(),
                entity_id.to_string(),
                segment_index,
            ));
        }

        for ((tenant, entity_type), signature) in &type_contracts {
            reconcile_key_contract_state(
                &mut tx,
                tenant,
                entity_type,
                signature.as_deref(),
                None,
                KeyContractUse::LiveWrite,
            )
            .await?;
        }
        for (tenant, entity_type) in &unreconciled_types {
            invalidate_key_coverage_for_unreconciled_append(&mut tx, tenant, entity_type).await?;
        }

        // ADR-0192: batch writes use the same exact declared-key contract as normal
        // actor appends. Delete every participating entity's old rows first so an
        // atomic batch may transfer ownership, then install each final set. This is
        // deliberately before journal insertion; any uniqueness error rolls the
        // entire transaction back without advancing any stream.
        for (append, (tenant, entity_type, entity_id, _)) in appends.iter().zip(parsed.iter()) {
            if !append.reconcile_keys {
                continue;
            }
            crate::dbm::postgres_query!(
                "DELETE FROM entity_key_index \
                 WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        }
        for (append, (tenant, entity_type, entity_id, _)) in appends.iter().zip(parsed.iter()) {
            if !append.reconcile_keys {
                continue;
            }
            let final_sequence = append
                .expected_sequence
                .checked_add(append.events.len() as u64)
                .ok_or_else(|| {
                    PersistenceError::Storage(format!(
                        "append_batch sequence overflow for '{}'",
                        append.persistence_id
                    ))
                })?;
            for key in &append.key_rows {
                crate::dbm::postgres_query!(
                    "INSERT INTO entity_key_index \
                     (tenant, entity_type, key_name, key_hash, entity_id, sequence_nr) \
                     VALUES ($1, $2, $3, $4, $5, $6)",
                )
                .bind(tenant)
                .bind(entity_type)
                .bind(&key.key_name)
                .bind(&key.key_hash)
                .bind(entity_id)
                .bind(final_sequence as i64)
                .execute(&mut *tx)
                .await
                .map_err(|e| PersistenceError::Storage(e.to_string()))?;
            }
        }

        let mut results = Vec::with_capacity(appends.len());
        for (append, (tenant, entity_type, entity_id, segment_index)) in
            appends.iter().zip(parsed.iter())
        {
            let mut new_seq = append.expected_sequence;
            for event in &append.events {
                new_seq += 1;
                let metadata_json = serde_json::to_value(&event.metadata)
                    .map_err(|e| PersistenceError::Serialization(e.to_string()))?;

                if let Err(e) = crate::dbm::postgres_query!(
                    "INSERT INTO events (tenant, entity_type, entity_id, sequence_nr, segment_index, event_type, payload, metadata) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                )
                .bind(tenant)
                .bind(entity_type)
                .bind(entity_id)
                .bind(new_seq as i64)
                .bind(*segment_index)
                .bind(&event.event_type)
                .bind(&event.payload)
                .bind(metadata_json)
                .execute(&mut *tx)
                .await
                {
                    let msg = e.to_string();
                    if msg.contains("unique") || msg.contains("duplicate key") {
                        transaction_timer.set_outcome("concurrency_violation");
                        return Err(PersistenceError::ConcurrencyViolation {
                            expected: append.expected_sequence,
                            actual: new_seq,
                        });
                    }
                    return Err(PersistenceError::Storage(msg));
                }
            }
            if new_seq > append.expected_sequence {
                segments::update_segment_after_append(
                    &mut tx,
                    tenant,
                    entity_type,
                    entity_id,
                    *segment_index,
                    new_seq,
                )
                .await?;
            }
            if append_retires_snapshot(
                append.expected_sequence,
                &append.events,
                &append.snapshot_source,
                entity_type,
                entity_id,
            ) {
                crate::dbm::postgres_query!(
                    "DELETE FROM snapshots \
                     WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
                )
                .bind(tenant)
                .bind(entity_type)
                .bind(entity_id)
                .execute(&mut *tx)
                .await
                .map_err(|error| PersistenceError::Storage(error.to_string()))?;
            }
            if !append.events.is_empty() {
                mark_query_projection_dirty(&mut tx, tenant, entity_type, entity_id).await?;
            }
            results.push(PersistenceAppendResult {
                persistence_id: append.persistence_id.clone(),
                sequence_nr: new_seq,
                batch_already_applied: false,
            });
        }

        if let Some(claim) = batch_claim {
            crate::dbm::postgres_query!(
                "INSERT INTO persistence_batch_idempotency \
                 (persistence_id, idempotency_key, intent_hash) VALUES ($1, $2, $3)",
            )
            .bind(&claim.persistence_id)
            .bind(&claim.idempotency_key)
            .bind(&claim.intent_hash)
            .execute(&mut *tx)
            .await
            .map_err(|error| PersistenceError::Storage(error.to_string()))?;
        }

        let commit_started = Instant::now();
        tx.commit().await.map_err(|e| {
            record_postgres_transaction_commit_duration(
                commit_started.elapsed(),
                EVENT_APPEND_OPERATION,
                "error",
            );
            PersistenceError::Storage(e.to_string())
        })?;
        record_postgres_transaction_commit_duration(
            commit_started.elapsed(),
            EVENT_APPEND_OPERATION,
            "ok",
        );
        transaction_timer.set_outcome("ok");

        Ok(results)
    }
}
