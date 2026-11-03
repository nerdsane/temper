//! Armed state timers (ADR-0056), kept so a restart arms them again.

use crate::driver::params;
use crate::metrics::TursoQueryTimer;
use temper_runtime::persistence::{PersistenceError, StateTimerRecord, storage_error};
use tracing::instrument;

use super::TursoEventStore;
use super::write_gate::WritePriority;

impl TursoEventStore {
    /// Record an armed timer. A row armed at a higher sequence is never
    /// replaced by one armed earlier, whatever order the writes arrive in.
    #[instrument(skip_all, fields(tenant = %timer.tenant, entity_type = %timer.entity_type, otel.name = "turso.upsert_state_timer"))]
    pub async fn upsert_state_timer(
        &self,
        timer: &StateTimerRecord,
    ) -> Result<(), PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.upsert_state_timer");
        let conn = self
            .write_connection("turso.upsert_state_timer", WritePriority::Low)
            .await?;
        conn.execute(
            "INSERT INTO state_timers \
                 (tenant, entity_type, entity_id, state, arm_id, sequence_nr, started_at, params, armed_by) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
             ON CONFLICT (tenant, entity_type, entity_id) DO UPDATE SET \
                 state = excluded.state, \
                 arm_id = excluded.arm_id, \
                 sequence_nr = excluded.sequence_nr, \
                 started_at = excluded.started_at, \
                 params = excluded.params, \
                 armed_by = excluded.armed_by \
             WHERE excluded.sequence_nr >= state_timers.sequence_nr",
            params![
                timer.tenant.as_str(),
                timer.entity_type.as_str(),
                timer.entity_id.as_str(),
                timer.state.as_str(),
                timer.arm_id.as_str(),
                i64::try_from(timer.sequence_nr).unwrap_or(i64::MAX),
                timer.started_at.as_str(),
                timer.params.as_str(),
                timer.armed_by.as_str(),
            ],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Remove an entity's timer row if it still holds the arming `arm_id`.
    #[instrument(skip_all, fields(tenant, entity_type, otel.name = "turso.delete_state_timer"))]
    pub async fn delete_state_timer(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> Result<(), PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.delete_state_timer");
        let conn = self
            .write_connection("turso.delete_state_timer", WritePriority::Low)
            .await?;
        conn.execute(
            "DELETE FROM state_timers \
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND arm_id = ?4",
            params![tenant, entity_type, entity_id, arm_id],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Every armed timer row for `tenant`.
    #[instrument(skip_all, fields(tenant, otel.name = "turso.list_state_timers"))]
    pub async fn list_state_timers(
        &self,
        tenant: &str,
    ) -> Result<Vec<StateTimerRecord>, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.list_state_timers");
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT entity_type, entity_id, state, arm_id, sequence_nr, started_at, params, armed_by \
                 FROM state_timers WHERE tenant = ?1",
                params![tenant],
            )
            .await
            .map_err(storage_error)?;
        let mut timers = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            timers.push(StateTimerRecord {
                tenant: tenant.to_string(),
                entity_type: row.get(0).map_err(storage_error)?,
                entity_id: row.get(1).map_err(storage_error)?,
                state: row.get(2).map_err(storage_error)?,
                arm_id: row.get(3).map_err(storage_error)?,
                sequence_nr: u64::try_from(row.get::<i64>(4).map_err(storage_error)?)
                    .unwrap_or_default(),
                started_at: row.get(5).map_err(storage_error)?,
                params: row.get(6).map_err(storage_error)?,
                armed_by: row.get(7).map_err(storage_error)?,
            });
        }
        Ok(timers)
    }

    /// One entity's timer row, if it has one.
    #[instrument(skip_all, fields(tenant, entity_type, otel.name = "turso.state_timer_row"))]
    pub async fn state_timer_row(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<Option<StateTimerRecord>, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.state_timer_row");
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT state, arm_id, sequence_nr, started_at, params, armed_by \
                 FROM state_timers WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                params![tenant, entity_type, entity_id],
            )
            .await
            .map_err(storage_error)?;
        let Some(row) = rows.next().await.map_err(storage_error)? else {
            return Ok(None);
        };
        Ok(Some(StateTimerRecord {
            tenant: tenant.to_string(),
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
            state: row.get(0).map_err(storage_error)?,
            arm_id: row.get(1).map_err(storage_error)?,
            sequence_nr: u64::try_from(row.get::<i64>(2).map_err(storage_error)?)
                .unwrap_or_default(),
            started_at: row.get(3).map_err(storage_error)?,
            params: row.get(4).map_err(storage_error)?,
            armed_by: row.get(5).map_err(storage_error)?,
        }))
    }
}
