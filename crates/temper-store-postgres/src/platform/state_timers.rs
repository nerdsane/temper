//! Armed state timers (ADR-0056), kept so a restart arms them again.

use temper_runtime::persistence::{PersistenceError, StateTimerRecord, storage_error};

use crate::PostgresEventStore;

type StateTimerColumns = (String, String, String, String, i64, String, String, String);

impl PostgresEventStore {
    /// Record an armed timer. A row armed at a higher sequence is never
    /// replaced by one armed earlier, whatever order the writes arrive in.
    pub async fn upsert_state_timer(
        &self,
        timer: &StateTimerRecord,
    ) -> Result<(), PersistenceError> {
        crate::dbm::postgres_query!(
            &self.schema,
            "INSERT INTO {schema}state_timers AS existing \
                 (tenant, entity_type, entity_id, state, arm_id, sequence_nr, started_at, params, armed_by) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             ON CONFLICT (tenant, entity_type, entity_id) DO UPDATE SET \
                 state = EXCLUDED.state, \
                 arm_id = EXCLUDED.arm_id, \
                 sequence_nr = EXCLUDED.sequence_nr, \
                 started_at = EXCLUDED.started_at, \
                 params = EXCLUDED.params, \
                 armed_by = EXCLUDED.armed_by \
             WHERE EXCLUDED.sequence_nr >= existing.sequence_nr",
        )
        .bind(&timer.tenant)
        .bind(&timer.entity_type)
        .bind(&timer.entity_id)
        .bind(&timer.state)
        .bind(&timer.arm_id)
        .bind(i64::try_from(timer.sequence_nr).unwrap_or(i64::MAX))
        .bind(&timer.started_at)
        .bind(&timer.params)
        .bind(&timer.armed_by)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Remove an entity's timer row if it still holds the arming `arm_id`.
    pub async fn delete_state_timer(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> Result<(), PersistenceError> {
        crate::dbm::postgres_query!(
            &self.schema,
            "DELETE FROM {schema}state_timers \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND arm_id = $4",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind(arm_id)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Every armed timer row for `tenant`.
    pub async fn list_state_timers(
        &self,
        tenant: &str,
    ) -> Result<Vec<StateTimerRecord>, PersistenceError> {
        let rows: Vec<StateTimerColumns> = crate::dbm::postgres_query_as!(
            &self.schema,
            "SELECT entity_type, entity_id, state, arm_id, sequence_nr, started_at, params, armed_by \
             FROM {schema}state_timers WHERE tenant = $1",
        )
        .bind(tenant)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows
            .into_iter()
            .map(
                |(
                    entity_type,
                    entity_id,
                    state,
                    arm_id,
                    sequence_nr,
                    started_at,
                    params,
                    armed_by,
                )| {
                    StateTimerRecord {
                        tenant: tenant.to_string(),
                        entity_type,
                        entity_id,
                        state,
                        arm_id,
                        sequence_nr: u64::try_from(sequence_nr).unwrap_or_default(),
                        started_at,
                        params,
                        armed_by,
                    }
                },
            )
            .collect())
    }
}
