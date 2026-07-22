//! Conditional PostgreSQL query-projection removal and repair.

use super::*;

impl PostgresEventStore {
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
        let mut tx = self.pool().begin().await.map_err(storage_error)?;
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
        let journal_matches =
            u64::try_from(journal_sequence).ok() == Some(source.expected_journal_sequence);
        let snapshot_matches =
            projection_snapshot_source_matches(&mut tx, tenant, entity_type, entity_id, source)
                .await?;
        if !journal_matches || !snapshot_matches {
            tx.commit().await.map_err(storage_error)?;
            return Ok(false);
        }
        clear_query_projection_dirty(&mut tx, tenant, entity_type, entity_id).await?;
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
        let mut transaction_timer =
            PostgresTransactionTimer::start(QUERY_PROJECTION_REMOVE_OPERATION);
        let acquire_started = Instant::now();
        let mut conn = match self.pool().acquire().await {
            Ok(conn) => {
                record_postgres_pool_acquire_duration(
                    acquire_started.elapsed(),
                    QUERY_PROJECTION_REMOVE_OPERATION,
                    "ok",
                );
                conn
            }
            Err(e) => {
                record_postgres_pool_acquire_duration(
                    acquire_started.elapsed(),
                    QUERY_PROJECTION_REMOVE_OPERATION,
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
                    QUERY_PROJECTION_REMOVE_OPERATION,
                    "ok",
                );
                tx
            }
            Err(e) => {
                record_postgres_transaction_begin_duration(
                    begin_started.elapsed(),
                    QUERY_PROJECTION_REMOVE_OPERATION,
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
            if !journal_matches || !snapshot_matches {
                tx.commit().await.map_err(storage_error)?;
                transaction_timer.set_outcome("source_changed");
                return Ok(false);
            }
        }
        let removed_catalog_sequence: Option<i64> = crate::dbm::postgres_query_scalar!(
            "DELETE FROM entity_catalog \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
             RETURNING sequence_nr",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage_error)?;
        let removed_fields = crate::dbm::postgres_query!("DELETE FROM entity_field_index WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3")
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        if removed_catalog_sequence.is_some() || removed_fields.rows_affected() > 0 {
            invalidate_key_coverage_for_derived_write(
                &mut tx,
                tenant,
                entity_type,
                entity_id,
                DerivedWriteSource::Catalog,
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
                QUERY_PROJECTION_REMOVE_OPERATION,
                "error",
            );
            storage_error(e)
        })?;
        record_postgres_transaction_commit_duration(
            commit_started.elapsed(),
            QUERY_PROJECTION_REMOVE_OPERATION,
            "ok",
        );
        transaction_timer.set_outcome("ok");
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
        let status = canonical_projection_status(status, state);
        let sequence_nr = i64::try_from(sequence_nr).map_err(|_| {
            PersistenceError::Storage("projection sequence exceeds PostgreSQL bigint".to_string())
        })?;
        let mut tx = self.pool().begin().await.map_err(storage_error)?;
        lock_key_contract(&mut tx, tenant, entity_type).await?;
        let stream_lock_key = event_stream_lock_key(tenant, entity_type, entity_id);
        lock_event_stream(&mut tx, &stream_lock_key).await?;
        let removed_catalog_sequence: Option<i64> = crate::dbm::postgres_query_scalar!(
            "DELETE FROM entity_catalog \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
               AND status = $4 AND fields = $5 AND state = $6 AND sequence_nr = $7 \
             RETURNING sequence_nr",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind(status)
        .bind(fields)
        .bind(state)
        .bind(sequence_nr)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage_error)?;
        if removed_catalog_sequence.is_some() {
            crate::dbm::postgres_query!(
                "DELETE FROM entity_field_index \
                 WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
            invalidate_key_coverage_for_derived_write(
                &mut tx,
                tenant,
                entity_type,
                entity_id,
                DerivedWriteSource::Catalog,
            )
            .await?;
            mark_query_projection_dirty(&mut tx, tenant, entity_type, entity_id).await?;
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(removed_catalog_sequence.is_some())
    }
}
