//! Transitional Turso tenant-store provider boundary.

use temper_runtime::persistence::PersistenceError;
use temper_store_turso::{TenantUserRow, TursoEventStore};

/// Explicit Turso tenant-store access for transitional boot/recovery paths.
#[async_trait::async_trait]
pub trait TursoStoreProvider: Send + Sync {
    fn supports_tenant_admin(&self) -> bool;
    fn platform_store(&self) -> Option<TursoEventStore>;
    async fn store_for_tenant(&self, tenant: &str) -> Option<TursoEventStore>;
    async fn all_stores(&self) -> Vec<TursoEventStore>;
    async fn connected_tenants(&self) -> Vec<String>;
    async fn tenants_for_user(&self, user_id: &str)
    -> Result<Vec<TenantUserRow>, PersistenceError>;
    async fn register_tenant(&self, tenant_id: &str) -> Result<TursoEventStore, PersistenceError>;
    async fn list_tenants(&self) -> Result<Vec<String>, PersistenceError>;
    async fn remove_tenant(&self, tenant_id: &str) -> Result<bool, PersistenceError>;
    async fn add_tenant_user(
        &self,
        tenant_id: &str,
        user_id: &str,
        role: &str,
    ) -> Result<(), PersistenceError>;
    async fn list_tenant_users(
        &self,
        tenant_id: &str,
    ) -> Result<Vec<TenantUserRow>, PersistenceError>;
    async fn remove_tenant_user(
        &self,
        tenant_id: &str,
        user_id: &str,
    ) -> Result<(), PersistenceError>;
    async fn ensure_tenant(&self, tenant_id: &str) -> Result<bool, PersistenceError>;
}
