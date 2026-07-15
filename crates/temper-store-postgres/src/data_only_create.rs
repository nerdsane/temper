//! Native storage path for brand-new data-only entity creates.

use std::time::Instant;

use sqlx::Acquire;
use temper_runtime::persistence::{EntityKeyRow, PersistenceEnvelope, PersistenceError};

use crate::PostgresEventStore;
use crate::metrics::{
    PostgresTransactionTimer, record_postgres_pool_acquire_duration,
    record_postgres_projection_index_fields, record_postgres_projection_index_reconciliation,
    record_postgres_transaction_begin_duration, record_postgres_transaction_commit_duration,
};
use crate::platform::{canonical_projection_status, json_hash, scalar_index_fields, storage_error};
use crate::store::{
    event_stream_lock_key, lock_event_stream, lock_key_contract, reconcile_key_contract_state,
};

const DATA_ONLY_CREATE_OPERATION: &str = "data_only_create";

impl PostgresEventStore {
    /// Atomically insert the first data-only event and its initial query projection.
    pub async fn create_data_only_entity_native(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        event: &PersistenceEnvelope,
    ) -> Result<u64, PersistenceError> {
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
            "total_event_count": 1,
            "sequence_nr": 1
        });
        self.create_data_only_entity_native_with_state(
            tenant,
            entity_type,
            entity_id,
            status,
            fields,
            &state,
            event,
        )
        .await
    }

    /// Atomically insert the first data-only event and its initial full-state query projection.
    #[expect(
        clippy::too_many_arguments,
        reason = "native data-only create storage boundary"
    )]
    pub async fn create_data_only_entity_native_with_state(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        state: &serde_json::Value,
        event: &PersistenceEnvelope,
    ) -> Result<u64, PersistenceError> {
        self.create_data_only_entity_native_with_state_and_keys(
            tenant,
            entity_type,
            entity_id,
            status,
            fields,
            state,
            event,
            &[],
            false,
            None,
        )
        .await
    }

    /// Atomically insert the first data-only event, query projection, and exact
    /// declared-key ownership set.
    #[expect(
        clippy::too_many_arguments,
        reason = "native data-only create storage boundary"
    )]
    pub async fn create_data_only_entity_native_with_state_and_keys(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        state: &serde_json::Value,
        event: &PersistenceEnvelope,
        key_rows: &[EntityKeyRow],
        reconcile_keys: bool,
        key_set_signature: Option<&str>,
    ) -> Result<u64, PersistenceError> {
        assert_eq!(
            event.sequence_nr, 1,
            "native data-only create requires first event sequence"
        );
        if !reconcile_keys && !key_rows.is_empty() {
            return Err(PersistenceError::Storage(
                "native data-only create supplied key rows without exact reconciliation"
                    .to_string(),
            ));
        }
        let status = canonical_projection_status(status, state);
        let projection_hash = json_hash(fields);
        let (new_index, indexed_fields, skipped_fields) = scalar_index_fields(fields);
        let mut field_names = Vec::with_capacity(new_index.len());
        let mut field_values = Vec::with_capacity(new_index.len());
        for (field_name, field_value) in &new_index {
            field_names.push(field_name.clone());
            field_values.push(field_value.clone());
        }

        let mut transaction_timer = PostgresTransactionTimer::start(DATA_ONLY_CREATE_OPERATION);
        let acquire_started = Instant::now();
        let mut conn = match self.pool().acquire().await {
            Ok(conn) => {
                record_postgres_pool_acquire_duration(
                    acquire_started.elapsed(),
                    DATA_ONLY_CREATE_OPERATION,
                    "ok",
                );
                conn
            }
            Err(e) => {
                record_postgres_pool_acquire_duration(
                    acquire_started.elapsed(),
                    DATA_ONLY_CREATE_OPERATION,
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
                    DATA_ONLY_CREATE_OPERATION,
                    "ok",
                );
                tx
            }
            Err(e) => {
                record_postgres_transaction_begin_duration(
                    begin_started.elapsed(),
                    DATA_ONLY_CREATE_OPERATION,
                    "error",
                );
                return Err(storage_error(e));
            }
        };

        lock_key_contract(&mut tx, tenant, entity_type).await?;
        let lock_key = event_stream_lock_key(tenant, entity_type, entity_id);
        lock_event_stream(&mut tx, &lock_key).await?;

        if reconcile_keys {
            for key in key_rows {
                let holder: Option<(String,)> = crate::dbm::postgres_query_as!(
                    "SELECT entity_id FROM entity_key_index \
                     WHERE tenant = $1 AND entity_type = $2 AND key_name = $3 AND key_hash = $4",
                )
                .bind(tenant)
                .bind(entity_type)
                .bind(&key.key_name)
                .bind(&key.key_hash)
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage_error)?;
                if let Some((existing,)) = holder
                    && existing != entity_id
                {
                    return Err(PersistenceError::Storage(format!(
                        "duplicate declared key '{}' for {entity_type}: held by {existing}",
                        key.key_name
                    )));
                }
            }
        }

        reconcile_key_contract_state(&mut tx, tenant, entity_type, key_set_signature).await?;

        let metadata_json = serde_json::to_value(&event.metadata)
            .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
        if let Err(e) = crate::dbm::postgres_query!(
            "INSERT INTO events \
             (tenant, entity_type, entity_id, sequence_nr, event_type, payload, metadata) \
             VALUES ($1, $2, $3, 1, $4, $5, $6)",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
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
                    expected: 0,
                    actual: 1,
                });
            }
            return Err(storage_error(e));
        }

        if let Err(e) = crate::dbm::postgres_query!(
            "INSERT INTO entity_catalog \
             (tenant, entity_type, entity_id, status, fields, state, sequence_nr, projection_version, projection_hash, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, 1, 2, $7, now())",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind(status)
        .bind(fields)
        .bind(state)
        .bind(projection_hash.as_str())
        .execute(&mut *tx)
        .await
        {
            let msg = e.to_string();
            if msg.contains("unique") || msg.contains("duplicate key") {
                transaction_timer.set_outcome("concurrency_violation");
                return Err(PersistenceError::ConcurrencyViolation {
                    expected: 0,
                    actual: 1,
                });
            }
            return Err(storage_error(e));
        }

        if !field_names.is_empty() {
            crate::dbm::postgres_query!(
                "INSERT INTO entity_field_index \
                 (tenant, entity_type, entity_id, field_name, field_value, status) \
                 SELECT $1, $2, $3, incoming.field_name, incoming.field_value, $6 \
                 FROM unnest($4::text[], $5::text[]) AS incoming(field_name, field_value)",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .bind(&field_names)
            .bind(&field_values)
            .bind(status)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        }

        if reconcile_keys {
            crate::dbm::postgres_query!(
                "DELETE FROM entity_key_index \
                 WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
            for key in key_rows {
                crate::dbm::postgres_query!(
                    "INSERT INTO entity_key_index \
                     (tenant, entity_type, key_name, key_hash, entity_id, sequence_nr) \
                     VALUES ($1, $2, $3, $4, $5, 1)",
                )
                .bind(tenant)
                .bind(entity_type)
                .bind(&key.key_name)
                .bind(&key.key_hash)
                .bind(entity_id)
                .execute(&mut *tx)
                .await
                .map_err(storage_error)?;
            }
        }

        let commit_started = Instant::now();
        tx.commit().await.map_err(|e| {
            record_postgres_transaction_commit_duration(
                commit_started.elapsed(),
                DATA_ONLY_CREATE_OPERATION,
                "error",
            );
            storage_error(e)
        })?;
        record_postgres_transaction_commit_duration(
            commit_started.elapsed(),
            DATA_ONLY_CREATE_OPERATION,
            "ok",
        );
        record_postgres_projection_index_fields(
            DATA_ONLY_CREATE_OPERATION,
            entity_type,
            indexed_fields,
            skipped_fields,
        );
        record_postgres_projection_index_reconciliation(
            DATA_ONLY_CREATE_OPERATION,
            "native_insert",
        );
        transaction_timer.set_outcome("ok");
        Ok(1)
    }
}
