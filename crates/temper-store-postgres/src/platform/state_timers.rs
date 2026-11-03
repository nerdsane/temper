//! Armed state timers (ADR-0056), kept so a restart arms them again.

use temper_runtime::persistence::{
    OverdueTimerRows, OverdueTimers, PersistenceError, StateTimerRecord, storage_error,
};

use crate::PostgresEventStore;

type StateTimerColumns = (
    String,
    String,
    String,
    String,
    i64,
    String,
    String,
    String,
    Option<String>,
);

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
                 (tenant, entity_type, entity_id, state, arm_id, sequence_nr, started_at, params, armed_by, due_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
             ON CONFLICT (tenant, entity_type, entity_id) DO UPDATE SET \
                 state = EXCLUDED.state, \
                 arm_id = EXCLUDED.arm_id, \
                 sequence_nr = EXCLUDED.sequence_nr, \
                 started_at = EXCLUDED.started_at, \
                 due_at = EXCLUDED.due_at, \
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
        .bind(&timer.due_at)
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

    /// Write the due time on an entity's timer row if it still holds the
    /// arming `arm_id`. Only an update: a row its timer removed stays removed.
    pub async fn date_state_timer(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
        due_at: &str,
    ) -> Result<(), PersistenceError> {
        crate::dbm::postgres_query!(
            &self.schema,
            "UPDATE {schema}state_timers SET due_at = $5 \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND arm_id = $4",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind(arm_id)
        .bind(due_at)
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
            "SELECT entity_type, entity_id, state, arm_id, sequence_nr, started_at, params, armed_by, due_at \
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
                    due_at,
                )| {
                    StateTimerRecord {
                        tenant: tenant.to_string(),
                        entity_type,
                        entity_id,
                        state,
                        arm_id,
                        sequence_nr: u64::try_from(sequence_nr).unwrap_or_default(),
                        started_at,
                        due_at,
                        params,
                        armed_by,
                    }
                },
            )
            .collect())
    }

    /// `tenant`'s state timers and scheduled actions due at or before `now`
    /// (RFC 3339 in UTC, the form their rows keep, compared as "C" so byte
    /// order is time order): how many, and the earliest due time. One
    /// statement, each half a range read of its table's `(tenant, due_at)`
    /// index, so it reads only the rows past due.
    pub async fn overdue_timer_rows(
        &self,
        tenant: &str,
        now: &str,
    ) -> Result<OverdueTimerRows, PersistenceError> {
        let rows: Vec<(i32, i64, Option<String>)> = crate::dbm::postgres_query_as!(
            &self.schema,
            "SELECT 0, COUNT(*), MIN(due_at COLLATE \"C\") FROM {schema}state_timers \
             WHERE tenant = $1 AND due_at COLLATE \"C\" <= $2 \
             UNION ALL \
             SELECT 1, COUNT(*), MIN(due_at COLLATE \"C\") FROM {schema}scheduled_actions \
             WHERE tenant = $1 AND due_at COLLATE \"C\" <= $2",
        )
        .bind(tenant)
        .bind(now)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        let mut overdue = OverdueTimerRows::default();
        for (kind, count, oldest_due_at) in rows {
            let rows = OverdueTimers {
                count: u64::try_from(count).unwrap_or_default(),
                oldest_due_at,
            };
            match kind {
                0 => overdue.state_timers = rows,
                _ => overdue.scheduled_actions = rows,
            }
        }
        Ok(overdue)
    }
}
