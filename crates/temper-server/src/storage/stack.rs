//! Boot-selected storage capability composition.

use super::*;

/// Composed storage capabilities selected at boot.
#[derive(Clone)]
pub struct StorageStack {
    pub backend: BackendLabel,
    pub events: BoxedEventStore,
    pub postgres_pool: Option<PgPool>,
    pub turso: Option<Arc<dyn TursoStoreProvider>>,
    pub platform: Option<Arc<dyn PlatformStore>>,
    pub policies: Option<Arc<dyn PolicyStore>>,
    pub query_plane: Option<Arc<dyn QueryPlaneStore>>,
    pub data_only_create: Option<Arc<dyn DataOnlyCreateStore>>,
    pub trajectory: Option<Arc<dyn TrajectorySink>>,
    pub metadata: Option<Arc<dyn MetadataStoreProvider>>,
}

impl StorageStack {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        backend: BackendLabel,
        events: BoxedEventStore,
        postgres_pool: Option<PgPool>,
        turso: Option<Arc<dyn TursoStoreProvider>>,
        platform: Option<Arc<dyn PlatformStore>>,
        policies: Option<Arc<dyn PolicyStore>>,
        query_plane: Option<Arc<dyn QueryPlaneStore>>,
        data_only_create: Option<Arc<dyn DataOnlyCreateStore>>,
        trajectory: Option<Arc<dyn TrajectorySink>>,
        metadata: Option<Arc<dyn MetadataStoreProvider>>,
    ) -> Self {
        Self {
            backend,
            events,
            postgres_pool,
            turso,
            platform,
            policies,
            query_plane,
            data_only_create,
            trajectory,
            metadata,
        }
    }

    pub fn from_postgres(store: PostgresEventStore) -> Self {
        let store = Arc::new(store);
        Self::new(
            BackendLabel::Postgres,
            BoxedEventStore::from_arc(store.clone()),
            Some(store.pool().clone()),
            None,
            Some(store.clone() as Arc<dyn PlatformStore>),
            Some(store.clone() as Arc<dyn PolicyStore>),
            Some(store.clone() as Arc<dyn QueryPlaneStore>),
            Some(store.clone() as Arc<dyn DataOnlyCreateStore>),
            Some(store.clone() as Arc<dyn TrajectorySink>),
            Some(Arc::new(SingleMetadataStoreProvider::new(store))),
        )
    }

    pub fn from_turso(store: TursoEventStore) -> Self {
        let store = Arc::new(store);
        Self::new(
            BackendLabel::Turso,
            BoxedEventStore::from_arc(store.clone()),
            None,
            Some(Arc::new(SingleTursoStoreProvider::new(store.clone()))),
            Some(store.clone() as Arc<dyn PlatformStore>),
            Some(store.clone() as Arc<dyn PolicyStore>),
            Some(store.clone() as Arc<dyn QueryPlaneStore>),
            None,
            Some(store.clone() as Arc<dyn TrajectorySink>),
            Some(Arc::new(SingleMetadataStoreProvider::new(store))),
        )
    }

    pub fn from_tenant_router(router: TenantStoreRouter) -> Self {
        let platform_store = Arc::new(router.platform_store().clone()) as Arc<dyn PlatformStore>;
        let router = Arc::new(router);
        Self::new(
            BackendLabel::TursoRouted,
            BoxedEventStore::from_arc(router.clone()),
            None,
            Some(Arc::new(TenantRoutedTursoStoreProvider::new(
                router.as_ref().clone(),
            ))),
            Some(platform_store),
            Some(router.clone() as Arc<dyn PolicyStore>),
            Some(router.clone() as Arc<dyn QueryPlaneStore>),
            None,
            Some(router.clone() as Arc<dyn TrajectorySink>),
            Some(Arc::new(TenantRoutedMetadataStoreProvider::new(
                router.as_ref().clone(),
            ))),
        )
    }

    pub fn from_redis(store: temper_store_redis::RedisEventStore) -> Self {
        let store = Arc::new(store);
        Self::new(
            BackendLabel::Redis,
            BoxedEventStore::from_arc(store),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
    }

    #[cfg(feature = "sim")]
    pub fn from_sim(
        store: temper_store_sim::SimEventStore,
        platform_store: Option<Arc<SimPlatformStore>>,
    ) -> Self {
        let store = Arc::new(store);
        let platform = platform_store.map(|store| store as Arc<dyn PlatformStore>);
        Self::new(
            BackendLabel::Sim,
            BoxedEventStore::from_arc(store),
            None,
            None,
            platform,
            None,
            None,
            None,
            None,
            None,
        )
    }
}

struct SingleMetadataStoreProvider {
    store: Arc<dyn MetadataStore>,
}

impl SingleMetadataStoreProvider {
    fn new<T>(store: Arc<T>) -> Self
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

struct SingleTursoStoreProvider {
    store: Arc<TursoEventStore>,
}

impl SingleTursoStoreProvider {
    fn new(store: Arc<TursoEventStore>) -> Self {
        Self { store }
    }
}

fn tenant_admin_unsupported() -> PersistenceError {
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

struct TenantRoutedMetadataStoreProvider {
    router: TenantStoreRouter,
}

impl TenantRoutedMetadataStoreProvider {
    fn new(router: TenantStoreRouter) -> Self {
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

struct TenantRoutedTursoStoreProvider {
    router: TenantStoreRouter,
}

impl TenantRoutedTursoStoreProvider {
    fn new(router: TenantStoreRouter) -> Self {
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
