//! Policy-store adapters for concrete storage backends.

use super::{PolicyGenerationWrite, PolicyStore, PolicyStoreRow};
use temper_store_postgres::{PostgresEventStore, PostgresPolicyGenerationWrite};
use temper_store_turso::{
    PolicyGenerationWrite as TursoPolicyGenerationWrite, TenantStoreRouter, TursoEventStore,
};

#[async_trait::async_trait]
impl PolicyStore for PostgresEventStore {
    async fn replace_policy_generation(
        &self,
        tenant: &str,
        entries: &[PolicyGenerationWrite],
        compatibility_text: &str,
    ) -> Result<(), String> {
        let entries = entries
            .iter()
            .map(|entry| PostgresPolicyGenerationWrite {
                policy_id: &entry.policy_id,
                cedar_text: &entry.cedar_text,
                enabled: entry.enabled,
                created_by: &entry.created_by,
            })
            .collect::<Vec<_>>();
        self.replace_policy_generation(tenant, &entries, compatibility_text)
            .await
            .map_err(|error| error.to_string())
    }

    async fn load_policy_compatibility_text(&self, tenant: &str) -> Result<Option<String>, String> {
        self.load_tenant_policy(tenant)
            .await
            .map_err(|error| error.to_string())
    }

    async fn save_policy(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        self.save_policy(tenant, policy_id, cedar_text, created_by)
            .await
            .map_err(|error| error.to_string())
    }

    async fn load_policies_for_tenant(&self, tenant: &str) -> Result<Vec<PolicyStoreRow>, String> {
        self.load_policies_for_tenant(tenant)
            .await
            .map(|rows| rows.into_iter().map(PolicyStoreRow::from).collect())
            .map_err(|error| error.to_string())
    }

    async fn load_all_policies(&self) -> Result<Vec<PolicyStoreRow>, String> {
        self.load_all_policies()
            .await
            .map(|rows| rows.into_iter().map(PolicyStoreRow::from).collect())
            .map_err(|error| error.to_string())
    }

    async fn toggle_policy_enabled(
        &self,
        tenant: &str,
        policy_id: &str,
        enabled: bool,
    ) -> Result<bool, String> {
        self.toggle_policy_enabled(tenant, policy_id, enabled)
            .await
            .map_err(|error| error.to_string())
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
            .map_err(|error| error.to_string())
    }

    async fn replace_policy(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        enabled: bool,
        created_by: &str,
    ) -> Result<bool, String> {
        self.replace_policy(tenant, policy_id, cedar_text, enabled, created_by)
            .await
            .map_err(|error| error.to_string())
    }

    async fn delete_policy(&self, tenant: &str, policy_id: &str) -> Result<(), String> {
        self.delete_policy(tenant, policy_id)
            .await
            .map_err(|error| error.to_string())
    }
}

#[async_trait::async_trait]
impl PolicyStore for TursoEventStore {
    async fn replace_policy_generation(
        &self,
        tenant: &str,
        entries: &[PolicyGenerationWrite],
        compatibility_text: &str,
    ) -> Result<(), String> {
        let entries = entries
            .iter()
            .map(|entry| TursoPolicyGenerationWrite {
                policy_id: &entry.policy_id,
                cedar_text: &entry.cedar_text,
                enabled: entry.enabled,
                created_by: &entry.created_by,
            })
            .collect::<Vec<_>>();
        self.replace_policy_generation(tenant, &entries, compatibility_text)
            .await
            .map_err(|error| error.to_string())
    }

    async fn load_policy_compatibility_text(&self, tenant: &str) -> Result<Option<String>, String> {
        self.load_tenant_policy(tenant)
            .await
            .map_err(|error| error.to_string())
    }

    async fn save_policy(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        self.save_policy(tenant, policy_id, cedar_text, created_by)
            .await
            .map_err(|error| error.to_string())
    }

    async fn load_policies_for_tenant(&self, tenant: &str) -> Result<Vec<PolicyStoreRow>, String> {
        self.load_policies_for_tenant(tenant)
            .await
            .map(|rows| rows.into_iter().map(PolicyStoreRow::from).collect())
            .map_err(|error| error.to_string())
    }

    async fn load_all_policies(&self) -> Result<Vec<PolicyStoreRow>, String> {
        self.load_all_policies()
            .await
            .map(|rows| rows.into_iter().map(PolicyStoreRow::from).collect())
            .map_err(|error| error.to_string())
    }

    async fn toggle_policy_enabled(
        &self,
        tenant: &str,
        policy_id: &str,
        enabled: bool,
    ) -> Result<bool, String> {
        self.toggle_policy_enabled(tenant, policy_id, enabled)
            .await
            .map_err(|error| error.to_string())
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
            .map_err(|error| error.to_string())
    }

    async fn replace_policy(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        enabled: bool,
        created_by: &str,
    ) -> Result<bool, String> {
        self.replace_policy(tenant, policy_id, cedar_text, enabled, created_by)
            .await
            .map_err(|error| error.to_string())
    }

    async fn delete_policy(&self, tenant: &str, policy_id: &str) -> Result<(), String> {
        self.delete_policy(tenant, policy_id)
            .await
            .map_err(|error| error.to_string())
    }
}

#[async_trait::async_trait]
impl PolicyStore for TenantStoreRouter {
    async fn replace_policy_generation(
        &self,
        tenant: &str,
        entries: &[PolicyGenerationWrite],
        compatibility_text: &str,
    ) -> Result<(), String> {
        let store = self
            .store_for_tenant(tenant)
            .await
            .map_err(|error| error.to_string())?;
        let entries = entries
            .iter()
            .map(|entry| TursoPolicyGenerationWrite {
                policy_id: &entry.policy_id,
                cedar_text: &entry.cedar_text,
                enabled: entry.enabled,
                created_by: &entry.created_by,
            })
            .collect::<Vec<_>>();
        store
            .replace_policy_generation(tenant, &entries, compatibility_text)
            .await
            .map_err(|error| error.to_string())
    }

    async fn load_policy_compatibility_text(&self, tenant: &str) -> Result<Option<String>, String> {
        let store = self
            .store_for_tenant(tenant)
            .await
            .map_err(|error| error.to_string())?;
        store
            .load_tenant_policy(tenant)
            .await
            .map_err(|error| error.to_string())
    }

    async fn save_policy(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        let store = self
            .store_for_tenant(tenant)
            .await
            .map_err(|error| error.to_string())?;
        store
            .save_policy(tenant, policy_id, cedar_text, created_by)
            .await
            .map_err(|error| error.to_string())
    }

    async fn load_policies_for_tenant(&self, tenant: &str) -> Result<Vec<PolicyStoreRow>, String> {
        let store = self
            .store_for_tenant(tenant)
            .await
            .map_err(|error| error.to_string())?;
        store
            .load_policies_for_tenant(tenant)
            .await
            .map(|rows| rows.into_iter().map(PolicyStoreRow::from).collect())
            .map_err(|error| error.to_string())
    }

    async fn load_all_policies(&self) -> Result<Vec<PolicyStoreRow>, String> {
        let mut rows: Vec<PolicyStoreRow> = self
            .platform_store()
            .load_all_policies()
            .await
            .map(|rows| rows.into_iter().map(PolicyStoreRow::from).collect())
            .map_err(|error| error.to_string())?;
        for tenant_id in self.connected_tenants().await {
            if let Ok(store) = self.store_for_tenant(&tenant_id).await {
                let mut tenant_rows: Vec<PolicyStoreRow> = store
                    .load_all_policies()
                    .await
                    .map(|rows| rows.into_iter().map(PolicyStoreRow::from).collect())
                    .map_err(|error| error.to_string())?;
                rows.append(&mut tenant_rows);
            }
        }
        Ok(rows)
    }

    async fn toggle_policy_enabled(
        &self,
        tenant: &str,
        policy_id: &str,
        enabled: bool,
    ) -> Result<bool, String> {
        let store = self
            .store_for_tenant(tenant)
            .await
            .map_err(|error| error.to_string())?;
        store
            .toggle_policy_enabled(tenant, policy_id, enabled)
            .await
            .map_err(|error| error.to_string())
    }

    async fn update_policy_text(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        let store = self
            .store_for_tenant(tenant)
            .await
            .map_err(|error| error.to_string())?;
        store
            .update_policy_text(tenant, policy_id, cedar_text, created_by)
            .await
            .map_err(|error| error.to_string())
    }

    async fn replace_policy(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        enabled: bool,
        created_by: &str,
    ) -> Result<bool, String> {
        let store = self
            .store_for_tenant(tenant)
            .await
            .map_err(|error| error.to_string())?;
        store
            .replace_policy(tenant, policy_id, cedar_text, enabled, created_by)
            .await
            .map_err(|error| error.to_string())
    }

    async fn delete_policy(&self, tenant: &str, policy_id: &str) -> Result<(), String> {
        let store = self
            .store_for_tenant(tenant)
            .await
            .map_err(|error| error.to_string())?;
        store
            .delete_policy(tenant, policy_id)
            .await
            .map_err(|error| error.to_string())
    }
}
