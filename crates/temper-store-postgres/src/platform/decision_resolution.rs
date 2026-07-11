//! Durable one-owner resolution transitions for pending decisions.

use temper_runtime::persistence::{PersistenceError, storage_error};

use crate::PostgresEventStore;

impl PostgresEventStore {
    /// Claim a pending decision for one stable resolution owner.
    pub async fn claim_decision_resolution(
        &self,
        tenant: &str,
        decision_id: &str,
        claimed_json: &str,
    ) -> Result<bool, PersistenceError> {
        let data: serde_json::Value = serde_json::from_str(claimed_json)
            .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
        let result = crate::dbm::postgres_query!(
            "UPDATE pending_decisions SET status = 'resolving', data = $3, updated_at = now() \
             WHERE tenant = $1 AND id = $2 AND status = 'pending'",
        )
        .bind(tenant)
        .bind(decision_id)
        .bind(data)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(result.rows_affected() == 1)
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
        let data: serde_json::Value = serde_json::from_str(decision_json)
            .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
        let result = crate::dbm::postgres_query!(
            "UPDATE pending_decisions SET status = $4, data = $5, updated_at = now() \
             WHERE tenant = $1 AND id = $2 AND status = 'resolving' \
               AND data ->> 'resolution_owner' = $3",
        )
        .bind(tenant)
        .bind(decision_id)
        .bind(owner)
        .bind(status)
        .bind(data)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(result.rows_affected() == 1)
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
