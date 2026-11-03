//! Replace a tenant's whole Cedar policy set in one transaction.

use temper_runtime::persistence::{PersistenceError, storage_error};

use super::compute_policy_hash;
use crate::PostgresEventStore;

impl PostgresEventStore {
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
        let mut transaction = self.pool().begin().await.map_err(storage_error)?;
        crate::dbm::postgres_query!(
            &self.schema,
            "INSERT INTO {schema}policies \
             (tenant, policy_id, cedar_text, policy_hash, created_at, created_by, enabled) \
             VALUES ($1, $2, $3, $4, now(), $5, true) \
             ON CONFLICT (tenant, policy_id) DO UPDATE SET \
                 cedar_text = EXCLUDED.cedar_text, \
                 policy_hash = EXCLUDED.policy_hash, \
                 created_by = EXCLUDED.created_by, \
                 created_at = now(), \
                 enabled = true",
        )
        .bind(tenant)
        .bind(policy_id)
        .bind(cedar_text)
        .bind(&policy_hash)
        .bind(created_by)
        .execute(&mut *transaction)
        .await
        .map_err(storage_error)?;
        crate::dbm::postgres_query!(
            &self.schema,
            "UPDATE {schema}policies SET enabled = false \
             WHERE tenant = $1 AND policy_id <> $2 AND enabled = true",
        )
        .bind(tenant)
        .bind(policy_id)
        .execute(&mut *transaction)
        .await
        .map_err(storage_error)?;
        transaction.commit().await.map_err(storage_error)
    }
}
