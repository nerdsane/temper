//! Durable one-owner resolution transitions for pending decisions.

use libsql::params;
use temper_runtime::persistence::{PersistenceError, storage_error};

use super::TursoEventStore;

impl TursoEventStore {
    /// Claim a pending decision for one stable resolution owner.
    pub async fn claim_decision_resolution(
        &self,
        tenant: &str,
        decision_id: &str,
        claimed_json: &str,
    ) -> Result<bool, PersistenceError> {
        let connection = self.configured_connection().await?;
        let affected = connection
            .execute(
                "UPDATE pending_decisions \
                 SET status = 'resolving', data = ?3, updated_at = datetime('now') \
                 WHERE tenant = ?1 AND id = ?2 AND status = 'pending'",
                params![tenant, decision_id, claimed_json],
            )
            .await
            .map_err(storage_error)?;
        Ok(affected == 1)
    }

    /// Persist progress or a terminal result for the exact winning owner.
    pub async fn update_decision_resolution(
        &self,
        tenant: &str,
        decision_id: &str,
        owner: &str,
        status: &str,
        decision_json: &str,
    ) -> Result<bool, PersistenceError> {
        let connection = self.configured_connection().await?;
        let affected = connection
            .execute(
                "UPDATE pending_decisions SET status = ?4, data = ?5, updated_at = datetime('now') \
                 WHERE tenant = ?1 AND id = ?2 AND status = 'resolving' \
                   AND json_extract(data, '$.resolution_owner') = ?3",
                params![tenant, decision_id, owner, status, decision_json],
            )
            .await
            .map_err(storage_error)?;
        Ok(affected == 1)
    }

    /// Release an unfinished owner back to pending after successful compensation.
    pub async fn release_decision_resolution(
        &self,
        tenant: &str,
        decision_id: &str,
        owner: &str,
        pending_json: &str,
    ) -> Result<bool, PersistenceError> {
        self.update_decision_resolution(tenant, decision_id, owner, "pending", pending_json)
            .await
    }
}
