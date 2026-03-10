//! WASM module registry for tracking deployed integration modules.
//!
//! Maps `(TenantId, module_name)` to SHA-256 hashes of compiled WASM modules.
//! The actual compiled modules are cached in the `WasmEngine` by hash.

use std::collections::BTreeMap;

use temper_runtime::tenant::TenantId;

/// Registry mapping tenant WASM module names to their compiled hashes.
///
/// Uses `BTreeMap` for deterministic iteration order (DST compliance).
#[derive(Debug, Clone, Default)]
pub struct WasmModuleRegistry {
    /// Maps (tenant, module_name) → sha256_hash.
    modules: BTreeMap<(String, String), String>,
    /// Built-in modules available to all tenants (module_name → sha256_hash).
    builtins: BTreeMap<String, String>,
}

impl WasmModuleRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a module hash for a tenant.
    pub fn register(&mut self, tenant: &TenantId, module_name: &str, sha256_hash: &str) {
        self.modules.insert(
            (tenant.to_string(), module_name.to_string()),
            sha256_hash.to_string(),
        );
    }

    /// Register a built-in module available to all tenants.
    pub fn register_builtin(&mut self, module_name: &str, sha256_hash: &str) {
        self.builtins
            .insert(module_name.to_string(), sha256_hash.to_string());
    }

    /// Look up the hash for a tenant's module, falling back to built-in modules.
    pub fn get_hash(&self, tenant: &TenantId, module_name: &str) -> Option<&str> {
        self.modules
            .get(&(tenant.to_string(), module_name.to_string()))
            .or_else(|| self.builtins.get(module_name))
            .map(|s| s.as_str())
    }

    /// Remove a module from the registry.
    pub fn remove(&mut self, tenant: &TenantId, module_name: &str) -> bool {
        self.modules
            .remove(&(tenant.to_string(), module_name.to_string()))
            .is_some()
    }

    /// List all modules for a tenant.
    pub fn modules_for_tenant(&self, tenant: &TenantId) -> Vec<(&str, &str)> {
        let tenant_str = tenant.to_string();
        self.modules
            .iter()
            .filter(|((t, _), _)| t == &tenant_str)
            .map(|((_, name), hash)| (name.as_str(), hash.as_str()))
            .collect()
    }

    /// Number of registered modules across all tenants.
    pub fn len(&self) -> usize {
        self.modules.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.modules.is_empty()
    }

    /// List all modules across all tenants (for observe cross-tenant views).
    pub fn all_modules(&self) -> Vec<(&str, &str, &str)> {
        self.modules
            .iter()
            .map(|((tenant, name), hash)| (tenant.as_str(), name.as_str(), hash.as_str()))
            .collect()
    }

    /// List all built-in modules (name, hash).
    pub fn all_builtins(&self) -> Vec<(&str, &str)> {
        self.builtins
            .iter()
            .map(|(name, hash)| (name.as_str(), hash.as_str()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_lookup() {
        let mut registry = WasmModuleRegistry::new();
        let tenant = TenantId::new("alpha");
        registry.register(&tenant, "stripe_charge", "abc123");

        assert_eq!(registry.get_hash(&tenant, "stripe_charge"), Some("abc123"));
        assert_eq!(registry.get_hash(&tenant, "unknown"), None);
    }

    #[test]
    fn remove_module() {
        let mut registry = WasmModuleRegistry::new();
        let tenant = TenantId::new("alpha");
        registry.register(&tenant, "stripe_charge", "abc123");

        assert!(registry.remove(&tenant, "stripe_charge"));
        assert!(!registry.remove(&tenant, "stripe_charge"));
        assert_eq!(registry.get_hash(&tenant, "stripe_charge"), None);
    }

    #[test]
    fn tenant_isolation() {
        let mut registry = WasmModuleRegistry::new();
        let alpha = TenantId::new("alpha");
        let beta = TenantId::new("beta");

        registry.register(&alpha, "stripe_charge", "hash-a");
        registry.register(&beta, "stripe_charge", "hash-b");

        assert_eq!(registry.get_hash(&alpha, "stripe_charge"), Some("hash-a"));
        assert_eq!(registry.get_hash(&beta, "stripe_charge"), Some("hash-b"));
    }

    #[test]
    fn modules_for_tenant_lists_correctly() {
        let mut registry = WasmModuleRegistry::new();
        let alpha = TenantId::new("alpha");
        let beta = TenantId::new("beta");

        registry.register(&alpha, "mod_a", "hash-a");
        registry.register(&alpha, "mod_b", "hash-b");
        registry.register(&beta, "mod_c", "hash-c");

        let alpha_modules = registry.modules_for_tenant(&alpha);
        assert_eq!(alpha_modules.len(), 2);

        let beta_modules = registry.modules_for_tenant(&beta);
        assert_eq!(beta_modules.len(), 1);
    }

    #[test]
    fn all_builtins_listed() {
        let mut registry = WasmModuleRegistry::new();
        registry.register_builtin("http_fetch", "builtin-hash-1");
        registry.register_builtin("email_send", "builtin-hash-2");

        let builtins = registry.all_builtins();
        assert_eq!(builtins.len(), 2);
        assert!(builtins.contains(&("email_send", "builtin-hash-2")));
        assert!(builtins.contains(&("http_fetch", "builtin-hash-1")));

        // Builtins should not appear in all_modules
        assert!(registry.all_modules().is_empty());
    }
}
