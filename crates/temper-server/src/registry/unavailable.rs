//! Tenants the registry knows about but cannot serve.
//!
//! A tenant whose stored CSDL or constraints do not load at startup is kept
//! out of the registry so every other tenant can still be served. It is
//! recorded here with the reason, so its requests answer with that state
//! instead of looking like a tenant that was never set up.

use std::collections::BTreeMap;

use temper_runtime::tenant::TenantId;

use super::SpecRegistry;

impl SpecRegistry {
    /// Record that `tenant` cannot be served, and why.
    ///
    /// The mark lasts for the life of this registry: the tenant's stored rows
    /// are what failed, so it clears when a later start restores them.
    pub fn mark_tenant_unavailable(&mut self, tenant: impl Into<TenantId>, reason: String) {
        assert!(!reason.is_empty(), "an unavailable tenant needs a reason");
        self.unavailable.insert(tenant.into(), reason);
    }

    /// Why `tenant` cannot be served, if it was marked unavailable.
    pub fn tenant_unavailable_reason(&self, tenant: &TenantId) -> Option<&str> {
        self.unavailable.get(tenant).map(String::as_str)
    }

    /// Every tenant marked unavailable, with its reason.
    pub fn unavailable_tenants(&self) -> &BTreeMap<TenantId, String> {
        &self.unavailable
    }
}
