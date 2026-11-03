//! Replace a tenant's whole Cedar policy set in one transaction.

use crate::driver::params;
use temper_runtime::persistence::{PersistenceError, storage_error};

use super::TursoEventStore;
use super::policy::compute_policy_hash;
use super::write_gate::WritePriority;

impl TursoEventStore {
    /// Make `policy_id` the tenant's only enabled entry, holding `cedar_text`.
    ///
    /// Every other entry of the tenant stays listed but is disabled, so a
    /// reload activates exactly the text that replaced the live set.
    pub async fn replace_tenant_policies(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<(), PersistenceError> {
        let policy_hash = compute_policy_hash(cedar_text);
        let connection = self
            .write_connection("turso.replace_tenant_policies", WritePriority::High)
            .await?;
        let transaction = connection.begin_immediate().await.map_err(storage_error)?;
        transaction
            .execute(
                "INSERT INTO policies \
                 (tenant, policy_id, cedar_text, policy_hash, created_at, created_by, enabled) \
                 VALUES (?1, ?2, ?3, ?4, datetime('now'), ?5, 1) \
                 ON CONFLICT(tenant, policy_id) DO UPDATE SET \
                     cedar_text = excluded.cedar_text, \
                     policy_hash = excluded.policy_hash, \
                     created_by = excluded.created_by, \
                     created_at = excluded.created_at, \
                     enabled = 1",
                params![tenant, policy_id, cedar_text, policy_hash, created_by],
            )
            .await
            .map_err(storage_error)?;
        transaction
            .execute(
                "UPDATE policies SET enabled = 0 \
                 WHERE tenant = ?1 AND policy_id <> ?2 AND enabled = 1",
                params![tenant, policy_id],
            )
            .await
            .map_err(storage_error)?;
        transaction.commit().await.map_err(storage_error)
    }
}
