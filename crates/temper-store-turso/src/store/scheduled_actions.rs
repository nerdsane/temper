//! Pending scheduled actions, kept so a restart arms them again. They are
//! written in the transaction of the event that armed them (`append_inner`).

use crate::driver::params;
use crate::metrics::TursoQueryTimer;
use temper_runtime::persistence::{PersistenceError, ScheduledActionRecord, storage_error};
use tracing::instrument;

use super::TursoEventStore;
use super::write_gate::WritePriority;

impl TursoEventStore {
    /// Durably cancel one scheduled action before garbage-collecting its row.
    #[instrument(skip_all, fields(tenant, entity_type, otel.name = "turso.cancel_scheduled_action"))]
    pub(crate) async fn cancel_scheduled_action_row(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> Result<(), PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.cancel_scheduled_action");
        let conn = self
            .write_connection("turso.cancel_scheduled_action", WritePriority::Low)
            .await?;
        conn.execute(
            "UPDATE scheduled_actions SET action = '' \
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND arm_id = ?4",
            params![tenant, entity_type, entity_id, arm_id],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Remove one scheduled action's row.
    #[instrument(skip_all, fields(tenant, entity_type, otel.name = "turso.delete_scheduled_action"))]
    pub(crate) async fn remove_scheduled_action(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> Result<(), PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.delete_scheduled_action");
        let conn = self
            .write_connection("turso.delete_scheduled_action", WritePriority::Low)
            .await?;
        conn.execute(
            "DELETE FROM scheduled_actions \
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND arm_id = ?4",
            params![tenant, entity_type, entity_id, arm_id],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Every pending scheduled action for `tenant`.
    #[instrument(skip_all, fields(tenant, otel.name = "turso.list_scheduled_actions"))]
    pub(crate) async fn scheduled_action_rows(
        &self,
        tenant: &str,
    ) -> Result<Vec<ScheduledActionRecord>, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.list_scheduled_actions");
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT entity_type, entity_id, arm_id, action, due_at, armed_by \
                 FROM scheduled_actions WHERE tenant = ?1",
                params![tenant],
            )
            .await
            .map_err(storage_error)?;
        let mut actions = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            actions.push(ScheduledActionRecord {
                tenant: tenant.to_string(),
                entity_type: row.get(0).map_err(storage_error)?,
                entity_id: row.get(1).map_err(storage_error)?,
                arm_id: row.get(2).map_err(storage_error)?,
                action: row.get(3).map_err(storage_error)?,
                due_at: row.get(4).map_err(storage_error)?,
                armed_by: row.get(5).map_err(storage_error)?,
            });
        }
        Ok(actions)
    }

    /// The pending scheduled actions of one record, by its key.
    #[instrument(skip_all, fields(tenant, entity_type, otel.name = "turso.list_entity_scheduled_actions"))]
    pub(crate) async fn entity_scheduled_action_rows(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<Vec<ScheduledActionRecord>, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.list_entity_scheduled_actions");
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT arm_id, action, due_at, armed_by FROM scheduled_actions \
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                params![tenant, entity_type, entity_id],
            )
            .await
            .map_err(storage_error)?;
        let mut actions = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            actions.push(ScheduledActionRecord {
                tenant: tenant.to_string(),
                entity_type: entity_type.to_string(),
                entity_id: entity_id.to_string(),
                arm_id: row.get(0).map_err(storage_error)?,
                action: row.get(1).map_err(storage_error)?,
                due_at: row.get(2).map_err(storage_error)?,
                armed_by: row.get(3).map_err(storage_error)?,
            });
        }
        Ok(actions)
    }
}
