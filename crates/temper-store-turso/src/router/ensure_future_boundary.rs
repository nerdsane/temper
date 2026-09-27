//! Expose tenant provisioning's Send contract without changing its future.

use std::future::Future;

use temper_runtime::persistence::PersistenceError;

use super::TenantStoreRouter;

impl TenantStoreRouter {
    /// Ensure a tenant exists in the persistence layer.
    ///
    /// If the tenant is already registered, returns `Ok(true)` (already existed).
    /// If not, provisions a new database and registers it, returning `Ok(false)`.
    pub fn ensure_tenant(
        &self,
        tenant_id: &str,
    ) -> impl Future<Output = Result<bool, PersistenceError>> + Send {
        // All lookup, provisioning, tracing and cancellation remain in the
        // unchanged async body and are deferred until this future is polled.
        self.ensure_tenant_inner(tenant_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send<T: Send>(_: &T) {}

    #[tokio::test]
    async fn dropping_unpolled_router_futures_does_not_provision_or_register() {
        let dir = tempfile::tempdir().expect("tempdir");
        let platform_url = format!("file:{}", dir.path().join("platform.db").display());
        let tenant_dir = dir.path().join("tenants");
        let router = TenantStoreRouter::new(
            &platform_url,
            None,
            Some(tenant_dir.to_string_lossy().to_string()),
        )
        .await
        .expect("router");
        assert!(!tenant_dir.exists());

        let ensure = router.ensure_tenant("unpolled-ensure");
        let register = router.register_tenant("unpolled-register");
        assert_send(&ensure);
        assert_send(&register);
        assert!(!tenant_dir.exists());
        drop(ensure);
        drop(register);

        assert!(
            !tenant_dir.exists(),
            "unpolled futures must not provision files"
        );
        assert!(router.connected_tenants().await.is_empty());
        assert!(
            router
                .list_tenants()
                .await
                .expect("list tenants")
                .is_empty()
        );
    }
}
