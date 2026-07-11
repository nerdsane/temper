//! Transactional persistence for Cedar policy approvals.

use libsql::{TransactionBehavior, params};
use temper_runtime::persistence::{PersistenceError, storage_error};

use super::TursoEventStore;
use super::policy::compute_policy_hash;
use crate::TursoPolicyApprovalCommit;

impl TursoEventStore {
    /// Atomically insert an approved policy and transition its decision.
    pub async fn commit_policy_approval(
        &self,
        commit: TursoPolicyApprovalCommit<'_>,
    ) -> Result<(), PersistenceError> {
        let TursoPolicyApprovalCommit {
            tenant,
            decision_id,
            approved_decision_json,
            policy_id,
            cedar_text,
            created_by,
        } = commit;
        let policy_hash = compute_policy_hash(cedar_text);
        let connection = self.configured_connection().await?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;

        let policy_rows = transaction
            .execute(
                "INSERT INTO policies \
                 (tenant, policy_id, cedar_text, policy_hash, created_at, created_by, enabled) \
                 VALUES (?1, ?2, ?3, ?4, datetime('now'), ?5, 1) \
                 ON CONFLICT(tenant, policy_id) DO NOTHING",
                params![tenant, policy_id, cedar_text, policy_hash, created_by],
            )
            .await
            .map_err(storage_error)?;
        if policy_rows != 1 {
            return Err(PersistenceError::Storage(format!(
                "policy approval '{policy_id}' already exists"
            )));
        }

        let decision_rows = transaction
            .execute(
                "UPDATE pending_decisions \
                 SET status = 'approved', data = ?3, updated_at = datetime('now') \
                 WHERE tenant = ?1 AND id = ?2 AND status = 'pending'",
                params![tenant, decision_id, approved_decision_json],
            )
            .await
            .map_err(storage_error)?;
        if decision_rows != 1 {
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
        let connection = self.configured_connection().await?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;

        transaction
            .execute(
                "DELETE FROM policies WHERE tenant = ?1 AND policy_id = ?2",
                params![tenant, policy_id],
            )
            .await
            .map_err(storage_error)?;

        let decision_rows = transaction
            .execute(
                "UPDATE pending_decisions \
                 SET status = 'pending', data = ?3, updated_at = datetime('now') \
                 WHERE tenant = ?1 AND id = ?2 AND status = 'approved'",
                params![tenant, decision_id, pending_decision_json],
            )
            .await
            .map_err(storage_error)?;
        if decision_rows != 1 {
            return Err(PersistenceError::Storage(format!(
                "approved decision '{decision_id}' was not available for rollback"
            )));
        }

        transaction.commit().await.map_err(storage_error)
    }
}
