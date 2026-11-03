//! Policy storage adapters for the configured durable backend.

use super::{PolicyStore, PolicyStoreRow, PostgresEventStore, TursoEventStore};

#[async_trait::async_trait]
impl PolicyStore for PostgresEventStore {
    async fn save_policy(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        self.save_policy(tenant, policy_id, cedar_text, created_by)
            .await
            .map_err(|e| e.to_string())
    }

    async fn load_policies_for_tenant(&self, tenant: &str) -> Result<Vec<PolicyStoreRow>, String> {
        self.load_policies_for_tenant(tenant)
            .await
            .map(|rows| rows.into_iter().map(PolicyStoreRow::from).collect())
            .map_err(|e| e.to_string())
    }

    async fn load_all_policies(&self) -> Result<Vec<PolicyStoreRow>, String> {
        self.load_all_policies()
            .await
            .map(|rows| rows.into_iter().map(PolicyStoreRow::from).collect())
            .map_err(|e| e.to_string())
    }

    async fn toggle_policy_enabled(
        &self,
        tenant: &str,
        policy_id: &str,
        enabled: bool,
    ) -> Result<bool, String> {
        self.toggle_policy_enabled(tenant, policy_id, enabled)
            .await
            .map_err(|e| e.to_string())
    }

    async fn update_policy_text(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        self.update_policy_text(tenant, policy_id, cedar_text, created_by)
            .await
            .map_err(|e| e.to_string())
    }

    /// Replace an enabled entry only if its stored hash still matches the reviewed version.
    /// Returns false for absent, disabled or changed entries; errors do not acknowledge a write.
    async fn replace_policy_if_hash(
        &self,
        tenant: &str,
        policy_id: &str,
        expected_hash: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        self.replace_policy_if_hash(tenant, policy_id, expected_hash, cedar_text, created_by)
            .await
            .map_err(|e| e.to_string())
    }

    async fn replace_tenant_policies(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<(), String> {
        self.replace_tenant_policies(tenant, policy_id, cedar_text, created_by)
            .await
            .map_err(|e| e.to_string())
    }

    async fn delete_policy(&self, tenant: &str, policy_id: &str) -> Result<(), String> {
        self.delete_policy(tenant, policy_id)
            .await
            .map_err(|e| e.to_string())
    }
}

#[async_trait::async_trait]
impl PolicyStore for TursoEventStore {
    async fn save_policy(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        self.save_policy(tenant, policy_id, cedar_text, created_by)
            .await
            .map_err(|e| e.to_string())
    }

    async fn load_policies_for_tenant(&self, tenant: &str) -> Result<Vec<PolicyStoreRow>, String> {
        self.load_policies_for_tenant(tenant)
            .await
            .map(|rows| rows.into_iter().map(PolicyStoreRow::from).collect())
            .map_err(|e| e.to_string())
    }

    async fn load_all_policies(&self) -> Result<Vec<PolicyStoreRow>, String> {
        self.load_all_policies()
            .await
            .map(|rows| rows.into_iter().map(PolicyStoreRow::from).collect())
            .map_err(|e| e.to_string())
    }

    async fn toggle_policy_enabled(
        &self,
        tenant: &str,
        policy_id: &str,
        enabled: bool,
    ) -> Result<bool, String> {
        self.toggle_policy_enabled(tenant, policy_id, enabled)
            .await
            .map_err(|e| e.to_string())
    }

    async fn update_policy_text(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        self.update_policy_text(tenant, policy_id, cedar_text, created_by)
            .await
            .map_err(|e| e.to_string())
    }

    /// Replace an enabled entry only if its stored hash still matches the reviewed version.
    /// Returns false for absent, disabled or changed entries; errors do not acknowledge a write.
    async fn replace_policy_if_hash(
        &self,
        tenant: &str,
        policy_id: &str,
        expected_hash: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        self.replace_policy_if_hash(tenant, policy_id, expected_hash, cedar_text, created_by)
            .await
            .map_err(|e| e.to_string())
    }

    async fn replace_tenant_policies(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<(), String> {
        self.replace_tenant_policies(tenant, policy_id, cedar_text, created_by)
            .await
            .map_err(|e| e.to_string())
    }

    async fn delete_policy(&self, tenant: &str, policy_id: &str) -> Result<(), String> {
        self.delete_policy(tenant, policy_id)
            .await
            .map_err(|e| e.to_string())
    }
}
