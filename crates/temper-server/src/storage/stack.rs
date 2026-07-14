//! Composed storage capabilities selected at boot (`StorageStack`).
//!
//! Extracted from `storage/mod.rs` (the workspace's largest file) when the
//! ARN-216 staleness-guard plumbing nudged it past the readability ratchet's
//! max-file-lines ceiling — a cohesive unit, moved verbatim.

use std::sync::Arc;

use super::{
    BackendLabel, BoxedEventStore, DataOnlyCreateStore, MetadataStoreProvider, PgPool,
    PlatformStore, PolicyStore, QueryPlaneStore, TrajectorySink, TursoStoreProvider,
};
use super::{
    SingleMetadataStoreProvider, SingleTursoStoreProvider, TenantRoutedMetadataStoreProvider,
    TenantRoutedTursoStoreProvider,
};
#[cfg(feature = "sim")]
use crate::platform_store::SimPlatformStore;
use temper_store_postgres::PostgresEventStore;
use temper_store_turso::{TenantStoreRouter, TursoEventStore};

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
