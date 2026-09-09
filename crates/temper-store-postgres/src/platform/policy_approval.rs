//! Transactional persistence for Cedar policy approvals.

use temper_runtime::persistence::{PersistenceError, storage_error};

use super::compute_policy_hash;
use crate::{PostgresEventStore, PostgresPolicyApprovalCommit};

impl PostgresEventStore {
    /// Atomically insert an approved policy and transition its decision.
    pub async fn commit_policy_approval(
        &self,
        commit: PostgresPolicyApprovalCommit<'_>,
    ) -> Result<(), PersistenceError> {
        let PostgresPolicyApprovalCommit {
            tenant,
            decision_id,
            approved_decision_json,
            policy_id,
            cedar_text,
            created_by,
        } = commit;
        let approved_data: serde_json::Value = serde_json::from_str(approved_decision_json)
            .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
        let policy_hash = compute_policy_hash(cedar_text);
        let mut transaction = self.pool().begin().await.map_err(storage_error)?;

        let policy_result = crate::dbm::postgres_query!(
            "INSERT INTO policies \
             (tenant, policy_id, cedar_text, policy_hash, created_at, created_by, enabled) \
             VALUES ($1, $2, $3, $4, now(), $5, true) \
             ON CONFLICT (tenant, policy_id) DO NOTHING",
        )
        .bind(tenant)
        .bind(policy_id)
        .bind(cedar_text)
        .bind(policy_hash)
        .bind(created_by)
        .execute(&mut *transaction)
        .await
        .map_err(storage_error)?;
        if policy_result.rows_affected() != 1 {
            return Err(PersistenceError::Storage(format!(
                "policy approval '{policy_id}' already exists"
            )));
        }

        let decision_result = crate::dbm::postgres_query!(
            "UPDATE pending_decisions \
             SET status = 'approved', data = $3, updated_at = now() \
             WHERE tenant = $1 AND id = $2 AND status = 'pending'",
        )
        .bind(tenant)
        .bind(decision_id)
        .bind(approved_data)
        .execute(&mut *transaction)
        .await
        .map_err(storage_error)?;
        if decision_result.rows_affected() != 1 {
            return Err(PersistenceError::Storage(format!(
                "pending decision '{decision_id}' was not available for approval"
            )));
        }

        transaction.commit().await.map_err(storage_error)
    }

    /// Compensate a committed approval when runtime activation fails.
    pub async fn rollback_policy_approval(
        &self,
        tenant: &str,
        decision_id: &str,
        pending_decision_json: &str,
        policy_id: &str,
    ) -> Result<(), PersistenceError> {
        let pending_data: serde_json::Value = serde_json::from_str(pending_decision_json)
            .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
        let mut transaction = self.pool().begin().await.map_err(storage_error)?;

        crate::dbm::postgres_query!("DELETE FROM policies WHERE tenant = $1 AND policy_id = $2",)
            .bind(tenant)
            .bind(policy_id)
            .execute(&mut *transaction)
            .await
            .map_err(storage_error)?;

        let decision_result = crate::dbm::postgres_query!(
            "UPDATE pending_decisions \
             SET status = 'pending', data = $3, updated_at = now() \
             WHERE tenant = $1 AND id = $2 AND status = 'approved'",
        )
        .bind(tenant)
        .bind(decision_id)
        .bind(pending_data)
        .execute(&mut *transaction)
        .await
        .map_err(storage_error)?;
        if decision_result.rows_affected() != 1 {
            return Err(PersistenceError::Storage(format!(
                "approved decision '{decision_id}' was not available for rollback"
            )));
        }

        transaction.commit().await.map_err(storage_error)
    }
}
