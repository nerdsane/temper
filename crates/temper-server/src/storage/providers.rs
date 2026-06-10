//! Backend-selection providers for the storage stack.
//!
//! Private wrappers adapting a single store or a tenant router to the
//! provider traits consumed by [`super::StorageStack`].

use std::sync::Arc;

use temper_runtime::persistence::PersistenceError;
use temper_store_turso::{TenantStoreRouter, TenantUserRow, TursoEventStore};

use super::{MetadataStore, MetadataStoreProvider, TursoStoreProvider};

pub(super) struct SingleMetadataStoreProvider {
    store: Arc<dyn MetadataStore>,
}

impl SingleMetadataStoreProvider {
    pub(super) fn new<T>(store: Arc<T>) -> Self
    where
        T: MetadataStore + 'static,
    {
        Self { store }
    }
}

#[async_trait::async_trait]
impl MetadataStoreProvider for SingleMetadataStoreProvider {
    fn platform_store(&self) -> Option<Arc<dyn MetadataStore>> {
        Some(self.store.clone())
    }

    async fn store_for_tenant(&self, _tenant: &str) -> Option<Arc<dyn MetadataStore>> {
        Some(self.store.clone())
    }

    async fn all_stores(&self) -> Vec<Arc<dyn MetadataStore>> {
        vec![self.store.clone()]
    }
}

pub(super) struct SingleTursoStoreProvider {
    store: Arc<TursoEventStore>,
}

impl SingleTursoStoreProvider {
    pub(super) fn new(store: Arc<TursoEventStore>) -> Self {
        Self { store }
    }
}

pub(super) fn tenant_admin_unsupported() -> PersistenceError {
    PersistenceError::Storage("tenant management requires routed Turso storage".to_string())
}

#[async_trait::async_trait]
impl TursoStoreProvider for SingleTursoStoreProvider {
    fn supports_tenant_admin(&self) -> bool {
        false
    }

    fn platform_store(&self) -> Option<TursoEventStore> {
        Some(self.store.as_ref().clone())
    }

    async fn store_for_tenant(&self, _tenant: &str) -> Option<TursoEventStore> {
        Some(self.store.as_ref().clone())
    }

    async fn all_stores(&self) -> Vec<TursoEventStore> {
        vec![self.store.as_ref().clone()]
    }

    async fn connected_tenants(&self) -> Vec<String> {
        Vec::new()
    }

    async fn tenants_for_user(
        &self,
        _user_id: &str,
    ) -> Result<Vec<TenantUserRow>, PersistenceError> {
        Err(tenant_admin_unsupported())
    }

    async fn register_tenant(&self, _tenant_id: &str) -> Result<TursoEventStore, PersistenceError> {
        Err(tenant_admin_unsupported())
    }

    async fn list_tenants(&self) -> Result<Vec<String>, PersistenceError> {
        Err(tenant_admin_unsupported())
    }

    async fn remove_tenant(&self, _tenant_id: &str) -> Result<bool, PersistenceError> {
        Err(tenant_admin_unsupported())
    }

    async fn add_tenant_user(
        &self,
        _tenant_id: &str,
        _user_id: &str,
        _role: &str,
    ) -> Result<(), PersistenceError> {
        Err(tenant_admin_unsupported())
    }

    async fn list_tenant_users(
        &self,
        _tenant_id: &str,
    ) -> Result<Vec<TenantUserRow>, PersistenceError> {
        Err(tenant_admin_unsupported())
    }

    async fn remove_tenant_user(
        &self,
        _tenant_id: &str,
        _user_id: &str,
    ) -> Result<(), PersistenceError> {
        Err(tenant_admin_unsupported())
    }

    async fn ensure_tenant(&self, _tenant_id: &str) -> Result<bool, PersistenceError> {
        Err(tenant_admin_unsupported())
    }
}

pub(super) struct TenantRoutedMetadataStoreProvider {
    router: TenantStoreRouter,
}

impl TenantRoutedMetadataStoreProvider {
    pub(super) fn new(router: TenantStoreRouter) -> Self {
        Self { router }
    }
}

#[async_trait::async_trait]
impl MetadataStoreProvider for TenantRoutedMetadataStoreProvider {
    fn platform_store(&self) -> Option<Arc<dyn MetadataStore>> {
        Some(Arc::new(self.router.platform_store().clone()) as Arc<dyn MetadataStore>)
    }

    async fn store_for_tenant(&self, tenant: &str) -> Option<Arc<dyn MetadataStore>> {
        self.router
            .store_for_tenant(tenant)
            .await
            .ok()
            .map(|store| Arc::new(store) as Arc<dyn MetadataStore>)
    }

    async fn all_stores(&self) -> Vec<Arc<dyn MetadataStore>> {
        let mut stores =
            vec![Arc::new(self.router.platform_store().clone()) as Arc<dyn MetadataStore>];
        for tenant_id in self.router.connected_tenants().await {
            if let Ok(store) = self.router.store_for_tenant(&tenant_id).await {
                stores.push(Arc::new(store) as Arc<dyn MetadataStore>);
            }
        }
        stores
    }
}

pub(super) struct TenantRoutedTursoStoreProvider {
    router: TenantStoreRouter,
}

impl TenantRoutedTursoStoreProvider {
    pub(super) fn new(router: TenantStoreRouter) -> Self {
        Self { router }
    }
}

#[async_trait::async_trait]
impl TursoStoreProvider for TenantRoutedTursoStoreProvider {
    fn supports_tenant_admin(&self) -> bool {
        true
    }

    fn platform_store(&self) -> Option<TursoEventStore> {
        Some(self.router.platform_store().clone())
    }

    async fn store_for_tenant(&self, tenant: &str) -> Option<TursoEventStore> {
        self.router.store_for_tenant(tenant).await.ok()
    }

    async fn all_stores(&self) -> Vec<TursoEventStore> {
        let mut stores = vec![self.router.platform_store().clone()];
        for tenant_id in self.router.connected_tenants().await {
            if let Ok(store) = self.router.store_for_tenant(&tenant_id).await {
                stores.push(store);
            }
        }
        stores
    }

    async fn connected_tenants(&self) -> Vec<String> {
        self.router.connected_tenants().await
    }

    async fn tenants_for_user(
        &self,
        user_id: &str,
    ) -> Result<Vec<TenantUserRow>, PersistenceError> {
        self.router.tenants_for_user(user_id).await
    }

    async fn register_tenant(&self, tenant_id: &str) -> Result<TursoEventStore, PersistenceError> {
        self.router.register_tenant(tenant_id).await
    }

    async fn list_tenants(&self) -> Result<Vec<String>, PersistenceError> {
        self.router.list_tenants().await
    }

    async fn remove_tenant(&self, tenant_id: &str) -> Result<bool, PersistenceError> {
        self.router.remove_tenant(tenant_id).await
    }

    async fn add_tenant_user(
        &self,
        tenant_id: &str,
        user_id: &str,
        role: &str,
    ) -> Result<(), PersistenceError> {
        self.router.add_tenant_user(tenant_id, user_id, role).await
    }

    async fn list_tenant_users(
        &self,
        tenant_id: &str,
    ) -> Result<Vec<TenantUserRow>, PersistenceError> {
        self.router.list_tenant_users(tenant_id).await
    }

    async fn remove_tenant_user(
        &self,
        tenant_id: &str,
        user_id: &str,
    ) -> Result<(), PersistenceError> {
        self.router.remove_tenant_user(tenant_id, user_id).await
    }

    async fn ensure_tenant(&self, tenant_id: &str) -> Result<bool, PersistenceError> {
        self.router.ensure_tenant(tenant_id).await
    }
}
