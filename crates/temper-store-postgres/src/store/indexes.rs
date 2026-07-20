//! Postgres event-store indexes operations.

use super::*;

impl PostgresEventStore {
    pub(super) async fn backfill_entity_keys(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        key_rows: &[temper_runtime::persistence::EntityKeyRow],
    ) -> Result<(), PersistenceError> {
        if key_rows.is_empty() {
            return Ok(());
        }
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        for key in key_rows {
            // A different entity already holding this key is a pre-existing data
            // conflict — log and skip (don't fail the whole backfill on one row;
            // the conflict surfaces via the metric and a keyed read still resolves
            // to whoever currently holds it).
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
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
            if let Some((existing,)) = &holder
                && existing != entity_id
            {
                tracing::warn!(
                    tenant, entity_type, entity_id, existing,
                    key_name = %key.key_name,
                    "entity_key_index backfill: declared-key conflict; skipping"
                );
                continue;
            }
            crate::dbm::postgres_query!(
                "DELETE FROM entity_key_index \
                 WHERE tenant = $1 AND entity_type = $2 AND key_name = $3 AND entity_id = $4",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(&key.key_name)
            .bind(entity_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
            crate::dbm::postgres_query!(
                "INSERT INTO entity_key_index \
                 (tenant, entity_type, key_name, key_hash, entity_id, sequence_nr) \
                 VALUES ($1, $2, $3, $4, $5, 0) \
                 ON CONFLICT (tenant, entity_type, key_name, key_hash) DO NOTHING",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(&key.key_name)
            .bind(&key.key_hash)
            .bind(entity_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        }
        tx.commit()
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        Ok(())
    }

    pub(super) async fn mark_key_index_backfilled(
        &self,
        tenant: &str,
        entity_type: &str,
        key_set: &str,
    ) -> Result<(), PersistenceError> {
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        // Upsert the covered key-set: a re-key after a key-set change must OVERWRITE the
        // stale set (not DO NOTHING) so `complete` reflects the keys actually assigned.
        crate::dbm::postgres_query!(
            "INSERT INTO key_index_backfill_watermark (tenant, entity_type, key_set) \
             VALUES ($1, $2, $3) \
             ON CONFLICT (tenant, entity_type) \
             DO UPDATE SET key_set = EXCLUDED.key_set, completed_at = now()",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(key_set)
        .execute(&mut *conn)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        Ok(())
    }

    pub(super) async fn key_index_backfilled_types(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        let rows: Vec<(String, String)> = crate::dbm::postgres_query_as!(
            "SELECT entity_type, key_set FROM key_index_backfill_watermark WHERE tenant = $1",
        )
        .bind(tenant)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        Ok(rows.into_iter().collect())
    }

    pub(super) async fn keyed_entity_ids_for_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        let rows: Vec<(String,)> = crate::dbm::postgres_query_as!(
            "SELECT DISTINCT entity_id FROM entity_key_index \
             WHERE tenant = $1 AND entity_type = $2",
        )
        .bind(tenant)
        .bind(entity_type)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        Ok(rows.into_iter().map(|(entity_id,)| entity_id).collect())
    }

    pub(super) async fn lookup_by_key(
        &self,
        tenant: &str,
        entity_type: &str,
        key_name: &str,
        key_hash: &str,
    ) -> Result<Option<String>, PersistenceError> {
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        let row: Option<(String,)> = crate::dbm::postgres_query_as!(
            "SELECT entity_id FROM entity_key_index \
             WHERE tenant = $1 AND entity_type = $2 AND key_name = $3 AND key_hash = $4",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(key_name)
        .bind(key_hash)
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        Ok(row.map(|(id,)| id))
    }

    pub(super) async fn backfill_entity_vectors(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        vector_rows: &[EntityVectorRow],
    ) -> Result<(), PersistenceError> {
        // Reconcile: DELETE all of the entity's rows, then insert the current ones.
        // Empty `vector_rows` purges the entity (deleted / un-embedded). Always runs
        // the delete (even for empty rows) so a purge is honored.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        crate::dbm::postgres_query!(
            "DELETE FROM entity_vector_index \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        for row in vector_rows {
            crate::dbm::postgres_query!(
                "INSERT INTO entity_vector_index \
                 (tenant, entity_type, decl_name, model_tag, entity_id, vector, sequence_nr) \
                 VALUES ($1, $2, $3, $4, $5, $6, 0)",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(&row.decl_name)
            .bind(&row.model_tag)
            .bind(entity_id)
            .bind(pack_f32_le(&row.vector))
            .execute(&mut *tx)
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        }
        tx.commit()
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        Ok(())
    }

    pub(super) async fn vector_candidates(
        &self,
        tenant: &str,
        entity_type: &str,
        decl_name: &str,
        model_tag: &str,
        limit: usize,
    ) -> Result<Vec<EntityVectorCandidate>, PersistenceError> {
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        let rows: Vec<(String, Vec<u8>)> = crate::dbm::postgres_query_as!(
            "SELECT entity_id, vector FROM entity_vector_index \
             WHERE tenant = $1 AND entity_type = $2 AND decl_name = $3 AND model_tag = $4 \
             ORDER BY entity_id LIMIT $5",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(decl_name)
        .bind(model_tag)
        .bind(limit as i64)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        Ok(rows
            .into_iter()
            .filter_map(|(entity_id, bytes)| {
                unpack_f32_le(&bytes).map(|vector| EntityVectorCandidate { entity_id, vector })
            })
            .collect())
    }

    pub(super) async fn mark_vector_index_backfilled(
        &self,
        tenant: &str,
        entity_type: &str,
        vector_set: &str,
    ) -> Result<(), PersistenceError> {
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        crate::dbm::postgres_query!(
            "INSERT INTO vector_index_backfill_watermark (tenant, entity_type, vector_set) \
             VALUES ($1, $2, $3) \
             ON CONFLICT (tenant, entity_type) \
             DO UPDATE SET vector_set = EXCLUDED.vector_set, completed_at = now()",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(vector_set)
        .execute(&mut *conn)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        Ok(())
    }

    pub(super) async fn vector_index_backfilled_types(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        let rows: Vec<(String, String)> = crate::dbm::postgres_query_as!(
            "SELECT entity_type, vector_set FROM vector_index_backfill_watermark WHERE tenant = $1",
        )
        .bind(tenant)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        Ok(rows.into_iter().collect())
    }

    pub(super) async fn vectored_entity_ids_for_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        let rows: Vec<(String,)> = crate::dbm::postgres_query_as!(
            "SELECT DISTINCT entity_id FROM entity_vector_index \
             WHERE tenant = $1 AND entity_type = $2",
        )
        .bind(tenant)
        .bind(entity_type)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        Ok(rows.into_iter().map(|(entity_id,)| entity_id).collect())
    }
}
