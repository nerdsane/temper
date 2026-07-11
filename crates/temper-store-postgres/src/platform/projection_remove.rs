//! Sequence-aware query-projection removal.

use super::*;

impl PostgresEventStore {
    /// Unconditionally remove both catalog and field-index projection rows.
    pub async fn remove_query_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), PersistenceError> {
        self.remove_query_projection_inner(tenant, entity_type, entity_id, None)
            .await
    }

    /// Remove a projection only when its catalog sequence is not newer than
    /// `sequence_nr`.
    pub async fn remove_query_projection_through_sequence(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        self.remove_query_projection_inner(
            tenant,
            entity_type,
            entity_id,
            Some(postgres_projection_sequence(
                sequence_nr,
                "projection removal",
            )?),
        )
        .await
    }

    async fn remove_query_projection_inner(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        through_sequence: Option<i64>,
    ) -> Result<(), PersistenceError> {
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
            Err(error) => {
                record_postgres_pool_acquire_duration(
                    acquire_started.elapsed(),
                    QUERY_PROJECTION_REMOVE_OPERATION,
                    "error",
                );
                return Err(storage_error(error));
            }
        };
        let begin_started = Instant::now();
        let mut transaction = match conn.begin().await {
            Ok(transaction) => {
                record_postgres_transaction_begin_duration(
                    begin_started.elapsed(),
                    QUERY_PROJECTION_REMOVE_OPERATION,
                    "ok",
                );
                transaction
            }
            Err(error) => {
                record_postgres_transaction_begin_duration(
                    begin_started.elapsed(),
                    QUERY_PROJECTION_REMOVE_OPERATION,
                    "error",
                );
                return Err(storage_error(error));
            }
        };
        if let Some(sequence_nr) = through_sequence {
            crate::dbm::postgres_query!(
                "DELETE FROM entity_field_index f USING entity_catalog c \
                 WHERE f.tenant = c.tenant AND f.entity_type = c.entity_type \
                   AND f.entity_id = c.entity_id \
                   AND c.tenant = $1 AND c.entity_type = $2 AND c.entity_id = $3 \
                   AND c.sequence_nr <= $4",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .bind(sequence_nr)
            .execute(&mut *transaction)
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
            .execute(&mut *transaction)
            .await
            .map_err(storage_error)?;
        } else {
            crate::dbm::postgres_query!(
                "DELETE FROM entity_field_index \
                 WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .execute(&mut *transaction)
            .await
            .map_err(storage_error)?;
            crate::dbm::postgres_query!(
                "DELETE FROM entity_catalog \
                 WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .execute(&mut *transaction)
            .await
            .map_err(storage_error)?;
        }
        let commit_started = Instant::now();
        transaction.commit().await.map_err(|error| {
            record_postgres_transaction_commit_duration(
                commit_started.elapsed(),
                QUERY_PROJECTION_REMOVE_OPERATION,
                "error",
            );
            storage_error(error)
        })?;
        record_postgres_transaction_commit_duration(
            commit_started.elapsed(),
            QUERY_PROJECTION_REMOVE_OPERATION,
            "ok",
        );
        transaction_timer.set_outcome("ok");
        Ok(())
    }
}
