//! Pauses the install's tenant and platform store calls, leaving record moves free.

use std::collections::BTreeMap;

use temper_runtime::persistence::PersistenceError;
use temper_server::platform_store::{
    InstalledAppRecord, ModuleReviewRow, PlatformStore, PolicyEntryRow, SpecRow,
    SpecVerificationUpdate, WasmModuleRow,
};
use temper_server::storage::TursoStoreProvider;
use temper_store_turso::{TenantUserRow, TursoEventStore};

use super::{Arc, INSTALLER, TenantId};

/// Holds the install at each storage call it makes for `tenant`, one release
/// at a time, so the test can move a record at every point it waits.
pub(super) struct PauseInstallStorage {
    pub(super) inner: Arc<dyn TursoStoreProvider>,
    pub(super) platform: Arc<dyn PlatformStore>,
    pub(super) tenant: TenantId,
    pub(super) entered: Arc<tokio::sync::Notify>,
    pub(super) release: Arc<tokio::sync::Semaphore>,
}

#[async_trait::async_trait]
impl TursoStoreProvider for PauseInstallStorage {
    fn supports_tenant_admin(&self) -> bool {
        self.inner.supports_tenant_admin()
    }
    fn platform_store(&self) -> Option<TursoEventStore> {
        // Route spec operations through the awaitable PlatformStore adapter
        // below, so pauses cover the gap after module registration too.
        None
    }
    async fn store_for_tenant(&self, tenant: &str) -> Option<TursoEventStore> {
        if tenant == self.tenant.as_str()
            && INSTALLER.try_with(|installer| *installer).unwrap_or(false)
        {
            self.pause().await;
        }
        self.inner.store_for_tenant(tenant).await
    }
    async fn all_stores(&self) -> Vec<TursoEventStore> {
        self.inner.all_stores().await
    }
    async fn connected_tenants(&self) -> Vec<String> {
        self.inner.connected_tenants().await
    }
    async fn tenants_for_user(
        &self,
        user_id: &str,
    ) -> Result<Vec<TenantUserRow>, PersistenceError> {
        self.inner.tenants_for_user(user_id).await
    }
    async fn register_tenant(&self, tenant_id: &str) -> Result<TursoEventStore, PersistenceError> {
        self.inner.register_tenant(tenant_id).await
    }
    async fn list_tenants(&self) -> Result<Vec<String>, PersistenceError> {
        self.inner.list_tenants().await
    }
    async fn remove_tenant(&self, tenant_id: &str) -> Result<bool, PersistenceError> {
        self.inner.remove_tenant(tenant_id).await
    }
    async fn add_tenant_user(
        &self,
        tenant_id: &str,
        user_id: &str,
        role: &str,
    ) -> Result<(), PersistenceError> {
        self.inner.add_tenant_user(tenant_id, user_id, role).await
    }
    async fn list_tenant_users(
        &self,
        tenant_id: &str,
    ) -> Result<Vec<TenantUserRow>, PersistenceError> {
        self.inner.list_tenant_users(tenant_id).await
    }
    async fn remove_tenant_user(
        &self,
        tenant_id: &str,
        user_id: &str,
    ) -> Result<(), PersistenceError> {
        self.inner.remove_tenant_user(tenant_id, user_id).await
    }
    async fn ensure_tenant(&self, tenant_id: &str) -> Result<bool, PersistenceError> {
        self.inner.ensure_tenant(tenant_id).await
    }
}

impl PauseInstallStorage {
    async fn pause(&self) {
        if INSTALLER.try_with(|installer| *installer).unwrap_or(false) {
            self.entered.notify_one();
            self.release.acquire().await.expect("release").forget();
        }
    }
}

#[async_trait::async_trait]
impl PlatformStore for PauseInstallStorage {
    async fn save_module_review(
        &self,
        tenant: &str,
        app_name: &str,
        version: &str,
        result: &str,
    ) -> Result<(), String> {
        self.platform
            .save_module_review(tenant, app_name, version, result)
            .await
    }

    async fn load_module_review(
        &self,
        tenant: &str,
        app_name: &str,
    ) -> Result<Option<ModuleReviewRow>, String> {
        self.platform.load_module_review(tenant, app_name).await
    }

    async fn upsert_spec(
        &self,
        tenant: &str,
        entity_type: &str,
        ioa_source: &str,
        csdl_xml: &str,
        content_hash: &str,
    ) -> Result<(), String> {
        self.pause().await;
        self.platform
            .upsert_spec(tenant, entity_type, ioa_source, csdl_xml, content_hash)
            .await
    }

    async fn load_specs(&self) -> Result<Vec<SpecRow>, String> {
        self.pause().await;
        self.platform.load_specs().await
    }

    async fn delete_spec(&self, tenant: &str, entity_type: &str) -> Result<(), String> {
        self.pause().await;
        self.platform.delete_spec(tenant, entity_type).await
    }

    async fn commit_specs(&self, tenant: &str) -> Result<(), String> {
        self.pause().await;
        self.platform.commit_specs(tenant).await
    }

    async fn commit_spec_versions(
        &self,
        tenant: &str,
        versions: &[(&str, &str, &str)],
        removed: &[&str],
    ) -> Result<bool, String> {
        self.pause().await;
        self.platform
            .commit_spec_versions(tenant, versions, removed)
            .await
    }

    async fn delete_uncommitted_specs(&self) -> Result<usize, String> {
        self.pause().await;
        self.platform.delete_uncommitted_specs().await
    }

    async fn load_verification_cache(
        &self,
        tenant: &str,
    ) -> Result<BTreeMap<String, (String, bool)>, String> {
        self.pause().await;
        self.platform.load_verification_cache(tenant).await
    }

    async fn persist_spec_verification(
        &self,
        tenant: &str,
        entity_type: &str,
        update: SpecVerificationUpdate<'_>,
    ) -> Result<(), String> {
        self.pause().await;
        self.platform
            .persist_spec_verification(tenant, entity_type, update)
            .await
    }

    async fn upsert_tenant_policy(&self, tenant: &str, policy_text: &str) -> Result<(), String> {
        self.pause().await;
        self.platform
            .upsert_tenant_policy(tenant, policy_text)
            .await
    }

    async fn upsert_tenant_constraints(
        &self,
        tenant: &str,
        cross_invariants_toml: &str,
    ) -> Result<(), String> {
        self.pause().await;
        self.platform
            .upsert_tenant_constraints(tenant, cross_invariants_toml)
            .await
    }

    async fn load_tenant_policies(&self) -> Result<Vec<(String, String)>, String> {
        self.pause().await;
        self.platform.load_tenant_policies().await
    }

    async fn load_policy_entries(&self) -> Result<Vec<PolicyEntryRow>, String> {
        self.pause().await;
        self.platform.load_policy_entries().await
    }

    async fn is_app_installed(&self, tenant: &str, app_name: &str) -> Result<bool, String> {
        self.pause().await;
        self.platform.is_app_installed(tenant, app_name).await
    }

    async fn record_installed_app(&self, tenant: &str, app_name: &str) -> Result<(), String> {
        self.pause().await;
        self.platform.record_installed_app(tenant, app_name).await
    }

    async fn record_installed_app_metadata(
        &self,
        record: &InstalledAppRecord,
    ) -> Result<(), String> {
        self.pause().await;
        self.platform.record_installed_app_metadata(record).await
    }

    async fn get_installed_app(
        &self,
        tenant: &str,
        app_name: &str,
    ) -> Result<Option<InstalledAppRecord>, String> {
        self.pause().await;
        self.platform.get_installed_app(tenant, app_name).await
    }

    async fn list_all_installed_apps(&self) -> Result<Vec<(String, String)>, String> {
        self.pause().await;
        self.platform.list_all_installed_apps().await
    }

    async fn upsert_pending_decision(
        &self,
        id: &str,
        tenant: &str,
        status: &str,
        data: &str,
    ) -> Result<(), String> {
        self.pause().await;
        self.platform
            .upsert_pending_decision(id, tenant, status, data)
            .await
    }

    async fn load_pending_decisions(&self, limit: usize) -> Result<Vec<String>, String> {
        self.pause().await;
        self.platform.load_pending_decisions(limit).await
    }

    async fn load_approved_session_decisions(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<Vec<String>, String> {
        self.pause().await;
        self.platform
            .load_approved_session_decisions(tenant, session_id)
            .await
    }

    async fn load_all_wasm_modules(&self, tenant: &str) -> Result<Vec<WasmModuleRow>, String> {
        self.pause().await;
        self.platform.load_all_wasm_modules(tenant).await
    }

    async fn load_wasm_modules_all_tenants(&self) -> Result<Vec<WasmModuleRow>, String> {
        self.pause().await;
        self.platform.load_wasm_modules_all_tenants().await
    }

    async fn upsert_wasm_module(
        &self,
        tenant: &str,
        name: &str,
        bytes: &[u8],
        hash: &str,
        source: &str,
    ) -> Result<(), String> {
        self.pause().await;
        self.platform
            .upsert_wasm_module(tenant, name, bytes, hash, source)
            .await
    }
}
