//! The tenants a boot restores after the server serves, and what each one
//! needs restored.
//!
//! A server restores its own tenants before it serves: `default`, the system
//! tenant, each `--app` tenant and the bootstrap closure's tenant. Every
//! other tenant in its store is left for after, so the time to serve does not
//! grow with the tenants it holds. What the boot reads for them before it
//! serves (each tenant's spec rows, its legacy policy text, its recorded app
//! installs and their cache roots) waits here, one read per store, until the
//! tenant's restore (`super::restore`) uses it.

use std::collections::{BTreeMap, BTreeSet};

use temper_platform::genesis_install::PlannedCacheRoot;
use temper_platform::state::PlatformState;
use temper_runtime::tenant::TenantId;
use temper_server::registry_bootstrap::StoredTenant;

/// The system tenant every server serves.
const SYSTEM_TENANT: &str = "temper-system";

/// The tenants left for after serving, by name, and the server's own.
pub(super) struct Later {
    own: BTreeSet<String>,
    pub(super) tenants: BTreeMap<String, LaterTenant>,
}

/// What one tenant left for after serving needs restored.
#[derive(Default)]
pub(super) struct LaterTenant {
    /// Its spec rows, read with every other tenant's.
    pub(super) specs: Option<StoredTenant>,
    /// Its specs are registered (or it was found unavailable).
    pub(super) registered: bool,
    /// It has a store of its own (a routed store), which holds its specs and
    /// is read when it is restored. Such a tenant also gets the platform's
    /// agent specs, as each connected tenant does at boot.
    pub(super) routed: bool,
    /// Its policy text from the legacy per-tenant table.
    pub(super) legacy_policies: Option<String>,
    /// The apps its recorded installs name, replayed as recorded.
    pub(super) installs: Vec<String>,
    /// The service-side installs into it whose files come back first.
    pub(super) cache_roots: Vec<PlannedCacheRoot>,
}

impl Later {
    /// No tenant left yet; `own` are the tenants restored before serving.
    pub(super) fn new(own: BTreeSet<String>) -> Self {
        Self {
            own,
            tenants: BTreeMap::new(),
        }
    }

    /// The tenants a server restores before it serves: `default`, the
    /// system tenant, each `--app` tenant, and `bootstrap_tenant` (the
    /// bootstrap closure's, `TEMPER_BOOTSTRAP_TENANT`).
    pub(super) fn own_tenants(
        apps: &[(String, String)],
        bootstrap_tenant: Option<&str>,
    ) -> BTreeSet<String> {
        let mut own: BTreeSet<String> = ["default", SYSTEM_TENANT]
            .into_iter()
            .map(str::to_string)
            .collect();
        own.extend(apps.iter().map(|(tenant, _)| tenant.clone()));
        own.extend(bootstrap_tenant.map(str::to_string));
        own
    }

    /// The server's own tenants.
    pub(super) fn own(&self) -> &BTreeSet<String> {
        &self.own
    }

    /// Whether `tenant` is one of the server's own.
    pub(super) fn is_own(&self, tenant: &str) -> bool {
        self.own.contains(tenant)
    }

    /// Whether `tenant` is restored after serving: left here already, or
    /// neither one of the server's own nor restored (registered or marked
    /// unavailable) before serving.
    pub(super) fn defers(&self, state: &PlatformState, tenant: &str) -> bool {
        if self.tenants.contains_key(tenant) {
            return true;
        }
        if self.is_own(tenant) || state.tenant_unavailable_reason(tenant).is_some() {
            return false;
        }
        let registry = state.registry.read().expect("registry lock poisoned");
        registry.get_tenant(&TenantId::new(tenant)).is_none()
    }

    /// What `tenant` needs restored after serving, leaving it for then.
    pub(super) fn tenant(&mut self, tenant: &str) -> &mut LaterTenant {
        assert!(!self.is_own(tenant), "{tenant} is restored before serving");
        self.tenants.entry(tenant.to_string()).or_default()
    }
}
