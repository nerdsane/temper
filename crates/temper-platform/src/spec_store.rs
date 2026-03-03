//! In-memory spec storage for tenant deploy workflows.
//!
//! [`SpecStore`] holds pre-authored specs (CSDL XML + IOA sources) keyed by
//! tenant name. When the DeploySpecs hook fires, it reads from this store
//! and feeds the specs into [`DeployPipeline::verify_and_deploy()`].

use std::collections::BTreeMap;

/// Maximum number of tenants stored simultaneously (TigerStyle budget).
const MAX_TENANTS: usize = 1_000;

/// Specs for a single tenant awaiting deployment.
#[derive(Debug, Clone)]
pub struct TenantSpecs {
    /// Raw CSDL XML schema for this tenant's entities.
    pub csdl_xml: String,
    /// IOA TOML sources keyed by entity type name.
    pub ioa_sources: BTreeMap<String, String>,
    /// WASM modules keyed by module name → wasm bytes.
    pub wasm_modules: BTreeMap<String, Vec<u8>>,
}

/// Bounded in-memory store for tenant specs pending deployment.
#[derive(Debug, Clone, Default)]
pub struct SpecStore {
    tenants: BTreeMap<String, TenantSpecs>,
}

impl SpecStore {
    /// Create an empty spec store.
    pub fn new() -> Self {
        Self {
            tenants: BTreeMap::new(),
        }
    }

    /// Store specs for a tenant. Returns `Err` if the budget is exhausted.
    pub fn store(&mut self, tenant: &str, specs: TenantSpecs) -> Result<(), String> {
        debug_assert!(
            self.tenants.len() <= MAX_TENANTS,
            "spec store budget exceeded"
        );
        if self.tenants.len() >= MAX_TENANTS && !self.tenants.contains_key(tenant) {
            return Err(format!(
                "spec store at capacity ({MAX_TENANTS} tenants); remove unused tenants first"
            ));
        }
        self.tenants.insert(tenant.to_string(), specs);
        Ok(())
    }

    /// Get specs for a tenant.
    pub fn get(&self, tenant: &str) -> Option<&TenantSpecs> {
        self.tenants.get(tenant)
    }

    /// Remove specs for a tenant (e.g. after successful deploy).
    pub fn remove(&mut self, tenant: &str) -> Option<TenantSpecs> {
        self.tenants.remove(tenant)
    }

    /// Number of tenants currently stored.
    pub fn len(&self) -> usize {
        self.tenants.len()
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.tenants.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_and_retrieve() {
        let mut store = SpecStore::new();
        let specs = TenantSpecs {
            csdl_xml: "<edmx/>".into(),
            ioa_sources: BTreeMap::from([("Task".into(), "ioa content".into())]),
            wasm_modules: BTreeMap::new(),
        };
        store.store("tenant-1", specs.clone()).unwrap();
        assert_eq!(store.len(), 1);
        assert!(store.get("tenant-1").is_some());
        assert_eq!(store.get("tenant-1").unwrap().csdl_xml, "<edmx/>");
    }

    #[test]
    fn remove_clears_entry() {
        let mut store = SpecStore::new();
        let specs = TenantSpecs {
            csdl_xml: "<edmx/>".into(),
            ioa_sources: BTreeMap::new(),
            wasm_modules: BTreeMap::new(),
        };
        store.store("t1", specs).unwrap();
        assert!(store.remove("t1").is_some());
        assert!(store.is_empty());
    }

    #[test]
    fn overwrite_existing_tenant() {
        let mut store = SpecStore::new();
        let specs1 = TenantSpecs {
            csdl_xml: "v1".into(),
            ioa_sources: BTreeMap::new(),
            wasm_modules: BTreeMap::new(),
        };
        let specs2 = TenantSpecs {
            csdl_xml: "v2".into(),
            ioa_sources: BTreeMap::new(),
            wasm_modules: BTreeMap::new(),
        };
        store.store("t1", specs1).unwrap();
        store.store("t1", specs2).unwrap();
        assert_eq!(store.len(), 1);
        assert_eq!(store.get("t1").unwrap().csdl_xml, "v2");
    }
}
