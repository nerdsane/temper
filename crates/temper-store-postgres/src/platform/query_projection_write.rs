//! Source-fenced PostgreSQL query-projection writes.

use super::*;

impl PostgresEventStore {
    pub async fn upsert_query_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        let state = serde_json::json!({
            "entity_type": entity_type,
            "entity_id": entity_id,
            "status": status,
            "item_count": 0,
            "counters": {},
            "booleans": {},
            "lists": {},
            "fields": fields,
            "events": [],
            "total_event_count": sequence_nr,
            "sequence_nr": sequence_nr
        });
        self.upsert_query_projection_with_state(
            tenant,
            entity_type,
            entity_id,
            status,
            fields,
            &state,
            sequence_nr,
        )
        .await
    }

    #[expect(clippy::too_many_arguments, reason = "projection upsert boundary")]
    pub async fn upsert_query_projection_with_state(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        state: &serde_json::Value,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        self.upsert_query_projection_with_state_inner(
            tenant,
            entity_type,
            entity_id,
            status,
            fields,
            state,
            sequence_nr,
            None,
        )
        .await
        .map(|_| ())
    }

    /// Upsert a projection only while its exact journal/snapshot source is current.
    #[expect(
        clippy::too_many_arguments,
        reason = "source-fenced projection boundary"
    )]
    pub async fn upsert_query_projection_with_state_if_source(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        state: &serde_json::Value,
        sequence_nr: u64,
        source: ProjectionSourceFence<'_>,
    ) -> Result<bool, PersistenceError> {
        self.upsert_query_projection_with_state_inner(
            tenant,
            entity_type,
            entity_id,
            status,
            fields,
            state,
            sequence_nr,
            Some(source),
        )
        .await
    }

    #[expect(clippy::too_many_arguments, reason = "projection upsert boundary")]
    async fn upsert_query_projection_with_state_inner(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        state: &serde_json::Value,
        sequence_nr: u64,
        source: Option<ProjectionSourceFence<'_>>,
    ) -> Result<bool, PersistenceError> {
        let source_fenced = source.is_some();
        let status = canonical_projection_status(status, state);
        let projection_hash = json_hash(fields);
        let (new_index, indexed_fields, skipped_fields) = scalar_index_fields(fields);
        let mut transaction_timer =
            PostgresTransactionTimer::start(QUERY_PROJECTION_UPSERT_OPERATION);
        let acquire_started = Instant::now();
        let mut conn = match self.pool().acquire().await {
            Ok(conn) => {
                record_postgres_pool_acquire_duration(
                    acquire_started.elapsed(),
                    QUERY_PROJECTION_UPSERT_OPERATION,
                    "ok",
                );
                conn
            }
            Err(e) => {
                record_postgres_pool_acquire_duration(
                    acquire_started.elapsed(),
                    QUERY_PROJECTION_UPSERT_OPERATION,
                    "error",
                );
                return Err(storage_error(e));
            }
        };
        let begin_started = Instant::now();
        let mut tx = match conn.begin().await {
            Ok(tx) => {
                record_postgres_transaction_begin_duration(
                    begin_started.elapsed(),
                    QUERY_PROJECTION_UPSERT_OPERATION,
                    "ok",
                );
                tx
            }
            Err(e) => {
                record_postgres_transaction_begin_duration(
                    begin_started.elapsed(),
                    QUERY_PROJECTION_UPSERT_OPERATION,
                    "error",
                );
                return Err(storage_error(e));
            }
        };

        lock_key_contract(&mut tx, tenant, entity_type).await?;
        let stream_lock_key = event_stream_lock_key(tenant, entity_type, entity_id);
        lock_event_stream(&mut tx, &stream_lock_key).await?;

        let journal_sequence: i64 = crate::dbm::postgres_query_scalar!(
            "SELECT COALESCE(MAX(sequence_nr), 0) FROM events \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(storage_error)?;

        let source_backed = if journal_sequence > 0 {
            true
        } else {
            crate::dbm::postgres_query_scalar!(
                "SELECT EXISTS(SELECT 1 FROM snapshots \
                 WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3)",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(storage_error)?
        };

        if let Some(source) = source {
            let journal_matches =
                u64::try_from(journal_sequence).ok() == Some(source.expected_journal_sequence);
            let snapshot_matches =
                projection_snapshot_source_matches(&mut tx, tenant, entity_type, entity_id, source)
                    .await?;
            let source_sequence = if source.expected_journal_sequence > 0 {
                source.expected_journal_sequence
            } else {
                source
                    .expected_snapshot
                    .map(|snapshot| snapshot.sequence_nr)
                    .unwrap_or(0)
            };
            if !journal_matches || !snapshot_matches || sequence_nr != source_sequence {
                tx.commit().await.map_err(storage_error)?;
                transaction_timer.set_outcome("source_changed");
                return Ok(false);
            }
        }

        let previous_catalog: Option<CatalogProjectionFingerprint> =
            crate::dbm::postgres_query_as!(
                "SELECT status, projection_hash, sequence_nr \
                 FROM entity_catalog \
                 WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
                 FOR UPDATE",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(storage_error)?;

        let new_sequence_nr = i64::try_from(sequence_nr).map_err(|_| {
            PersistenceError::Storage("projection sequence exceeds PostgreSQL bigint".to_string())
        })?;
        let incoming_is_stale = if source_fenced {
            false
        } else if journal_sequence > 0 {
            new_sequence_nr != journal_sequence
        } else {
            previous_catalog
                .as_ref()
                .is_some_and(|(_, _, existing_sequence)| *existing_sequence > new_sequence_nr)
        };
        let previous_catalog = if incoming_is_stale {
            let commit_started = Instant::now();
            tx.commit().await.map_err(|e| {
                record_postgres_transaction_commit_duration(
                    commit_started.elapsed(),
                    QUERY_PROJECTION_UPSERT_OPERATION,
                    "error",
                );
                storage_error(e)
            })?;
            record_postgres_transaction_commit_duration(
                commit_started.elapsed(),
                QUERY_PROJECTION_UPSERT_OPERATION,
                "ok",
            );
            record_postgres_projection_index_fields(
                QUERY_PROJECTION_UPSERT_OPERATION,
                entity_type,
                indexed_fields,
                skipped_fields,
            );
            record_postgres_projection_index_reconciliation(
                QUERY_PROJECTION_UPSERT_OPERATION,
                "stale_skipped",
            );
            transaction_timer.set_outcome("stale_skipped");
            return Ok(false);
        } else if previous_catalog.is_some() {
            update_query_projection_catalog_row(
                &mut tx,
                QueryProjectionCatalogUpdate {
                    tenant,
                    entity_type,
                    entity_id,
                    status,
                    fields,
                    state,
                    sequence_nr,
                    projection_hash: projection_hash.as_str(),
                },
            )
            .await?;
            previous_catalog
        } else {
            let inserted: Option<i32> = crate::dbm::postgres_query_scalar!(
                "INSERT INTO entity_catalog \
                 (tenant, entity_type, entity_id, status, fields, state, sequence_nr, projection_version, projection_hash, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, 2, $8, now()) \
                 ON CONFLICT (tenant, entity_type, entity_id) DO NOTHING \
                 RETURNING 1",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .bind(status)
            .bind(fields)
            .bind(state)
            .bind(sequence_nr as i64)
            .bind(projection_hash.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(storage_error)?;

            if inserted.is_some() {
                None
            } else {
                let raced_catalog: Option<CatalogProjectionFingerprint> =
                    crate::dbm::postgres_query_as!(
                        "SELECT status, projection_hash, sequence_nr \
                         FROM entity_catalog \
                         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
                         FOR UPDATE",
                    )
                    .bind(tenant)
                    .bind(entity_type)
                    .bind(entity_id)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(storage_error)?;
                let raced_is_stale = if source_fenced {
                    false
                } else if journal_sequence > 0 {
                    new_sequence_nr != journal_sequence
                } else {
                    raced_catalog
                        .as_ref()
                        .is_some_and(|(_, _, existing_sequence)| {
                            *existing_sequence > new_sequence_nr
                        })
                };
                if raced_is_stale {
                    let commit_started = Instant::now();
                    tx.commit().await.map_err(|e| {
                        record_postgres_transaction_commit_duration(
                            commit_started.elapsed(),
                            QUERY_PROJECTION_UPSERT_OPERATION,
                            "error",
                        );
                        storage_error(e)
                    })?;
                    record_postgres_transaction_commit_duration(
                        commit_started.elapsed(),
                        QUERY_PROJECTION_UPSERT_OPERATION,
                        "ok",
                    );
                    record_postgres_projection_index_fields(
                        QUERY_PROJECTION_UPSERT_OPERATION,
                        entity_type,
                        indexed_fields,
                        skipped_fields,
                    );
                    record_postgres_projection_index_reconciliation(
                        QUERY_PROJECTION_UPSERT_OPERATION,
                        "stale_skipped",
                    );
                    transaction_timer.set_outcome("stale_skipped");
                    return Ok(false);
                }
                update_query_projection_catalog_row(
                    &mut tx,
                    QueryProjectionCatalogUpdate {
                        tenant,
                        entity_type,
                        entity_id,
                        status,
                        fields,
                        state,
                        sequence_nr,
                        projection_hash: projection_hash.as_str(),
                    },
                )
                .await?;
                raced_catalog
            }
        };

        let catalog_changed =
            previous_catalog
                .as_ref()
                .is_none_or(|(old_status, old_hash, old_sequence)| {
                    old_status.as_str() != status
                        || old_hash.as_str() != projection_hash.as_str()
                        || *old_sequence != new_sequence_nr
                });
        if catalog_changed {
            invalidate_key_coverage_for_derived_write(
                &mut tx,
                tenant,
                entity_type,
                entity_id,
                DerivedWriteSource::Catalog,
            )
            .await?;
        }

        let should_reconcile_index =
            previous_catalog
                .as_ref()
                .is_none_or(|(old_status, old_hash, _)| {
                    old_status.as_str() != status || old_hash.as_str() != projection_hash.as_str()
                });
        let reconciliation_path = if should_reconcile_index {
            if previous_catalog.is_some() {
                "diff"
            } else {
                "insert"
            }
        } else {
            "skipped_unchanged"
        };

        if should_reconcile_index {
            reconcile_query_projection_field_index(
                &mut tx,
                tenant,
                entity_type,
                entity_id,
                status,
                &new_index,
            )
            .await?;
        }

        if source_fenced {
            clear_query_projection_dirty(&mut tx, tenant, entity_type, entity_id).await?;
        } else if source_backed {
            mark_query_projection_dirty(&mut tx, tenant, entity_type, entity_id).await?;
        } else {
            clear_query_projection_dirty(&mut tx, tenant, entity_type, entity_id).await?;
        }

        let commit_started = Instant::now();
        tx.commit().await.map_err(|e| {
            record_postgres_transaction_commit_duration(
                commit_started.elapsed(),
                QUERY_PROJECTION_UPSERT_OPERATION,
                "error",
            );
            storage_error(e)
        })?;
        record_postgres_transaction_commit_duration(
            commit_started.elapsed(),
            QUERY_PROJECTION_UPSERT_OPERATION,
            "ok",
        );
        record_postgres_projection_index_fields(
            QUERY_PROJECTION_UPSERT_OPERATION,
            entity_type,
            indexed_fields,
            skipped_fields,
        );
        record_postgres_projection_index_reconciliation(
            QUERY_PROJECTION_UPSERT_OPERATION,
            reconciliation_path,
        );
        transaction_timer.set_outcome("ok");
        Ok(true)
    }
}
