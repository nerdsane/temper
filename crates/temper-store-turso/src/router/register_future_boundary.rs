//! Check registration's Send proof once, separately from ensure_tenant.

use std::future::Future;

use temper_runtime::persistence::PersistenceError;

use super::TenantStoreRouter;
use crate::TursoEventStore;

impl TenantStoreRouter {
    /// Register and connect a new tenant.
    ///
    /// In local mode, creates a new SQLite file in `local_base_dir`.
    /// In cloud mode (with `cloud` feature), provisions via Turso Cloud API.
    pub fn register_tenant(
        &self,
        tenant_id: &str,
    ) -> impl Future<Output = Result<TursoEventStore, PersistenceError>> + Send {
        self.register_tenant_inner(tenant_id)
    }
}
