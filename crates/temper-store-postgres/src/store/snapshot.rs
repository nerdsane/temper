//! Source-fenced PostgreSQL snapshot persistence.

use super::*;

impl PostgresEventStore {
    pub(super) async fn save_snapshot_inner(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
        source: &SnapshotSourceFence,
        key_contract: Option<&str>,
    ) -> Result<(), PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let sequence_nr = i64::try_from(sequence_nr).map_err(|_| {
            PersistenceError::Storage("snapshot sequence exceeds PostgreSQL bigint".to_string())
        })?;

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| PersistenceError::Storage(error.to_string()))?;

        lock_key_contract(&mut tx, tenant, entity_type).await?;
        let lock_key = event_stream_lock_key(tenant, entity_type, entity_id);
        lock_event_stream(&mut tx, &lock_key).await?;
        if let Some(key_contract) = key_contract {
            reconcile_key_contract_state(
                &mut tx,
                tenant,
                entity_type,
                Some(key_contract),
                None,
                KeyContractUse::LiveWrite,
            )
            .await?;
        }

        let previous = load_snapshot_for_update(&mut tx, tenant, entity_type, entity_id).await?;
        if !snapshot_source_matches(previous.as_ref(), source) {
            return Err(PersistenceError::SnapshotGenerationChanged);
        }
        let journal_generation: (i64, Option<String>, Option<serde_json::Value>) =
            crate::dbm::postgres_query_as!(
                "SELECT COALESCE(MAX(sequence_nr), 0), \
               (SELECT event_type FROM events \
                WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND sequence_nr = 1), \
               (SELECT payload FROM events \
                WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND sequence_nr = 1) \
             FROM events \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|error| PersistenceError::Storage(error.to_string()))?;
        if matches!(source, SnapshotSourceFence::Unchecked)
            && journal_generation.0 > 0
            && journal_generation.2.as_ref().is_some_and(|payload| {
                is_state_materialization_payload_for(
                    journal_generation.1.as_deref().unwrap_or_default(),
                    payload,
                    entity_type,
                    entity_id,
                )
            })
        {
            tx.commit()
                .await
                .map_err(|error| PersistenceError::Storage(error.to_string()))?;
            return Ok(());
        }
        let snapshot_is_noop = previous.as_ref().is_some_and(|(stored_sequence, stored)| {
            *stored_sequence > sequence_nr
                || (*stored_sequence == sequence_nr && stored.as_slice() == snapshot)
        });
        if snapshot_is_noop {
            tx.commit()
                .await
                .map_err(|error| PersistenceError::Storage(error.to_string()))?;
            return Ok(());
        }
        let same_sequence_replacement = previous
            .as_ref()
            .is_some_and(|(stored_sequence, _)| *stored_sequence == sequence_nr);

        if key_contract.is_none() {
            invalidate_key_coverage_for_derived_write(
                &mut tx,
                tenant,
                entity_type,
                entity_id,
                DerivedWriteSource::Snapshot,
            )
            .await?;
        }

        crate::dbm::postgres_query!(
            "INSERT INTO snapshots (tenant, entity_type, entity_id, sequence_nr, state) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (tenant, entity_type, entity_id) \
             DO UPDATE SET sequence_nr = $4, state = $5, created_at = now()",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind(sequence_nr)
        .bind(snapshot)
        .execute(&mut *tx)
        .await
        .map_err(|error| PersistenceError::Storage(error.to_string()))?;

        crate::dbm::postgres_query!(
            "INSERT INTO snapshot_history (tenant, entity_type, entity_id, sequence_nr, state) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (tenant, entity_type, entity_id, sequence_nr) \
             DO UPDATE SET state = $5, created_at = now()",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind(sequence_nr)
        .bind(snapshot)
        .execute(&mut *tx)
        .await
        .map_err(|error| PersistenceError::Storage(error.to_string()))?;

        if !same_sequence_replacement {
            segments::rotate_after_snapshot(
                &mut tx,
                tenant,
                entity_type,
                entity_id,
                sequence_nr as u64,
            )
            .await?;
        }

        mark_query_projection_dirty(&mut tx, tenant, entity_type, entity_id).await?;

        tx.commit()
            .await
            .map_err(|error| PersistenceError::Storage(error.to_string()))?;
        Ok(())
    }
}
