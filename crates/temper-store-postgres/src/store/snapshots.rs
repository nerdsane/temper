//! Postgres event-store snapshots operations.

use super::*;

impl PostgresEventStore {
    /// Save (upsert) a snapshot for the given entity.
    ///
    /// Uses `ON CONFLICT … DO UPDATE` so that only the latest snapshot is
    /// retained per entity.
    pub(super) async fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        crate::dbm::postgres_query!(
            "INSERT INTO snapshots (tenant, entity_type, entity_id, sequence_nr, state) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (tenant, entity_type, entity_id) \
             DO UPDATE SET sequence_nr = $4, state = $5, created_at = now()",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind(sequence_nr as i64)
        .bind(snapshot)
        .execute(&mut *tx)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        crate::dbm::postgres_query!(
            "INSERT INTO snapshot_history (tenant, entity_type, entity_id, sequence_nr, state) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (tenant, entity_type, entity_id, sequence_nr) \
             DO UPDATE SET state = $5, created_at = now()",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind(sequence_nr as i64)
        .bind(snapshot)
        .execute(&mut *tx)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        segments::rotate_after_snapshot(&mut tx, tenant, entity_type, entity_id, sequence_nr)
            .await?;

        tx.commit()
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        Ok(())
    }

    pub(super) async fn replace_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        expected_snapshot: &[u8],
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| PersistenceError::Storage(error.to_string()))?;

        let updated = crate::dbm::postgres_query!(
            "UPDATE snapshots SET state = $5, created_at = now() \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
               AND sequence_nr = $4 AND state = $6",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind(sequence_nr as i64)
        .bind(snapshot)
        .bind(expected_snapshot)
        .execute(&mut *tx)
        .await
        .map_err(|error| PersistenceError::Storage(error.to_string()))?;
        if updated.rows_affected() != 1 {
            return Err(PersistenceError::Storage(format!(
                "snapshot changed or is missing at sequence {sequence_nr} for {persistence_id}"
            )));
        }

        crate::dbm::postgres_query!(
            "INSERT INTO snapshot_history (tenant, entity_type, entity_id, sequence_nr, state) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (tenant, entity_type, entity_id, sequence_nr) \
             DO UPDATE SET state = $5, created_at = now()",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind(sequence_nr as i64)
        .bind(snapshot)
        .execute(&mut *tx)
        .await
        .map_err(|error| PersistenceError::Storage(error.to_string()))?;

        tx.commit()
            .await
            .map_err(|error| PersistenceError::Storage(error.to_string()))?;
        Ok(())
    }

    /// Load the latest snapshot for an entity.
    ///
    /// Returns `None` when no snapshot has been saved yet.
    pub(super) async fn load_snapshot(
        &self,
        persistence_id: &str,
    ) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;

        let row: Option<(i64, Vec<u8>)> = crate::dbm::postgres_query_as!(
            "SELECT sequence_nr, state FROM snapshots \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        Ok(row.map(|(seq, state)| (seq as u64, state)))
    }

    /// List all distinct entities that have at least one persisted event
    /// in the given tenant.
    pub(super) async fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        let rows: Vec<(String, String)> = crate::dbm::postgres_query_as!(
            "SELECT DISTINCT e.entity_type, e.entity_id \
             FROM events e \
             WHERE e.tenant = $1 \
               AND NOT EXISTS ( \
                 SELECT 1 FROM events d \
                 WHERE d.tenant = e.tenant \
                   AND d.entity_type = e.entity_type \
                   AND d.entity_id = e.entity_id \
                   AND (d.payload ->> 'to_status' = 'Deleted' \
                     OR (d.event_type = 'Deleted' \
                       AND jsonb_typeof(d.payload -> 'to_status') IS DISTINCT FROM 'string')) \
               )",
        )
        .bind(tenant)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        Ok(rows)
    }

    /// List distinct entity IDs for one entity type in the given tenant.
    pub(super) async fn list_entity_ids_by_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        let rows: Vec<String> = crate::dbm::postgres_query_scalar!(
            "SELECT entity_id \
             FROM ( \
               SELECT c.entity_id \
               FROM entity_catalog c \
               WHERE c.tenant = $1 \
                 AND c.entity_type = $2 \
                 AND NOT EXISTS ( \
                   SELECT 1 \
                   FROM events d \
                   WHERE d.tenant = c.tenant \
                     AND d.entity_type = c.entity_type \
                     AND d.entity_id = c.entity_id \
                     AND (d.payload ->> 'to_status' = 'Deleted' \
                       OR (d.event_type = 'Deleted' \
                         AND jsonb_typeof(d.payload -> 'to_status') IS DISTINCT FROM 'string')) \
                 ) \
               UNION \
               SELECT f.entity_id \
               FROM entity_field_index f \
               WHERE f.tenant = $1 \
                 AND f.entity_type = $2 \
                 AND NOT EXISTS ( \
                   SELECT 1 \
                   FROM events d \
                   WHERE d.tenant = f.tenant \
                     AND d.entity_type = f.entity_type \
                     AND d.entity_id = f.entity_id \
                     AND (d.payload ->> 'to_status' = 'Deleted' \
                       OR (d.event_type = 'Deleted' \
                         AND jsonb_typeof(d.payload -> 'to_status') IS DISTINCT FROM 'string')) \
                 ) \
               UNION \
               SELECT DISTINCT e.entity_id \
               FROM events e \
               WHERE e.tenant = $1 \
                 AND e.entity_type = $2 \
                 AND NOT EXISTS ( \
                   SELECT 1 \
                   FROM events d \
                   WHERE d.tenant = e.tenant \
                     AND d.entity_type = e.entity_type \
                     AND d.entity_id = e.entity_id \
                     AND (d.payload ->> 'to_status' = 'Deleted' \
                       OR (d.event_type = 'Deleted' \
                         AND jsonb_typeof(d.payload -> 'to_status') IS DISTINCT FROM 'string')) \
                 ) \
             ) ids \
             ORDER BY entity_id",
        )
        .bind(tenant)
        .bind(entity_type)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        Ok(rows)
    }

    pub(super) async fn list_entity_ids_limited(
        &self,
        tenant: &str,
        entity_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let limit = limit.min(i64::MAX as usize) as i64;
        if let Some(entity_type) = entity_type {
            let rows: Vec<(String, String)> = crate::dbm::postgres_query_as!(
                "SELECT DISTINCT e.entity_type, e.entity_id \
                 FROM events e \
                 WHERE e.tenant = $1 AND e.entity_type = $2 \
                   AND NOT EXISTS ( \
                     SELECT 1 FROM events d \
                     WHERE d.tenant = e.tenant \
                       AND d.entity_type = e.entity_type \
                       AND d.entity_id = e.entity_id \
                       AND (d.payload ->> 'to_status' = 'Deleted' \
                         OR (d.event_type = 'Deleted' \
                           AND jsonb_typeof(d.payload -> 'to_status') IS DISTINCT FROM 'string')) \
                   ) \
                 ORDER BY e.entity_type, e.entity_id \
                 LIMIT $3",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
            return Ok(rows);
        }

        let rows: Vec<(String, String)> = crate::dbm::postgres_query_as!(
            "SELECT DISTINCT e.entity_type, e.entity_id \
             FROM events e \
             WHERE e.tenant = $1 \
               AND NOT EXISTS ( \
                 SELECT 1 FROM events d \
                 WHERE d.tenant = e.tenant \
                   AND d.entity_type = e.entity_type \
                   AND d.entity_id = e.entity_id \
                   AND (d.payload ->> 'to_status' = 'Deleted' \
                     OR (d.event_type = 'Deleted' \
                       AND jsonb_typeof(d.payload -> 'to_status') IS DISTINCT FROM 'string')) \
               ) \
             ORDER BY e.entity_type, e.entity_id \
             LIMIT $2",
        )
        .bind(tenant)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        Ok(rows)
    }
}
