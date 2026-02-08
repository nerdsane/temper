//! Per-tenant specification registry.
//!
//! The [`SpecRegistry`] maps `(TenantId, EntityType)` to parsed specifications
//! and transition tables. It replaces the flat `HashMap<String, TransitionTable>`
//! in `ServerState`, enabling multi-tenant deployments where each tenant has
//! its own entity types and specs.

use std::collections::BTreeMap;
use std::sync::Arc;

use temper_jit::table::TransitionTable;
use temper_runtime::tenant::TenantId;
use temper_spec::automaton::{self, Automaton};
use temper_spec::csdl::CsdlDocument;

/// A registered tenant with its specs and entity configuration.
#[derive(Debug, Clone)]
pub struct TenantConfig {
    /// The CSDL document describing this tenant's entity model.
    pub csdl: Arc<CsdlDocument>,
    /// Raw CSDL XML for serving via `$metadata`.
    pub csdl_xml: Arc<String>,
    /// Maps entity set names to entity type names (from CSDL).
    pub entity_set_map: BTreeMap<String, String>,
    /// Per-entity-type specs.
    pub entities: BTreeMap<String, EntitySpec>,
}

/// A registered entity type's spec and transition table.
#[derive(Debug, Clone)]
pub struct EntitySpec {
    /// The parsed I/O Automaton specification.
    pub automaton: Automaton,
    /// The compiled transition table (ready for evaluation).
    pub table: Arc<TransitionTable>,
    /// Raw IOA TOML source (for invariant parsing, display, etc.).
    pub ioa_source: String,
}

/// Multi-tenant specification registry.
///
/// Thread-safe for concurrent reads. Registration is done at startup;
/// hot-swap via [`SwapController`](temper_jit::SwapController) can update
/// individual tables without replacing the entire registry.
#[derive(Debug, Clone, Default)]
pub struct SpecRegistry {
    tenants: BTreeMap<TenantId, TenantConfig>,
}

impl SpecRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tenant with its CSDL document and IOA specs.
    ///
    /// `ioa_sources` maps entity type name to IOA TOML source string.
    /// Each source is parsed into an [`Automaton`] and compiled into a
    /// [`TransitionTable`].
    pub fn register_tenant(
        &mut self,
        tenant: impl Into<TenantId>,
        csdl: CsdlDocument,
        csdl_xml: String,
        ioa_sources: &[(&str, &str)],
    ) {
        let tenant = tenant.into();

        // Build entity set map from CSDL
        let mut entity_set_map = BTreeMap::new();
        for schema in &csdl.schemas {
            for container in &schema.entity_containers {
                for entity_set in &container.entity_sets {
                    let type_name = entity_set
                        .entity_type
                        .rsplit('.')
                        .next()
                        .unwrap_or(&entity_set.entity_type);
                    entity_set_map.insert(entity_set.name.clone(), type_name.to_string());
                }
            }
        }

        // Parse and compile each IOA spec
        let mut entities = BTreeMap::new();
        for (entity_type, ioa_source) in ioa_sources {
            let automaton = automaton::parse_automaton(ioa_source)
                .unwrap_or_else(|e| panic!("failed to parse IOA for {entity_type}: {e}"));
            let table = TransitionTable::from_automaton(&automaton);

            entities.insert(
                entity_type.to_string(),
                EntitySpec {
                    automaton,
                    table: Arc::new(table),
                    ioa_source: ioa_source.to_string(),
                },
            );
        }

        self.tenants.insert(
            tenant,
            TenantConfig {
                csdl: Arc::new(csdl),
                csdl_xml: Arc::new(csdl_xml),
                entity_set_map,
                entities,
            },
        );
    }

    /// Look up a tenant's configuration.
    pub fn get_tenant(&self, tenant: &TenantId) -> Option<&TenantConfig> {
        self.tenants.get(tenant)
    }

    /// Look up a transition table for a specific tenant and entity type.
    pub fn get_table(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Option<Arc<TransitionTable>> {
        self.tenants
            .get(tenant)
            .and_then(|tc| tc.entities.get(entity_type))
            .map(|es| es.table.clone())
    }

    /// Look up the entity type name for an entity set in a tenant.
    pub fn resolve_entity_type(
        &self,
        tenant: &TenantId,
        entity_set: &str,
    ) -> Option<String> {
        self.tenants
            .get(tenant)
            .and_then(|tc| tc.entity_set_map.get(entity_set).cloned())
    }

    /// Look up the IOA spec for a tenant and entity type.
    pub fn get_spec(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Option<&EntitySpec> {
        self.tenants
            .get(tenant)
            .and_then(|tc| tc.entities.get(entity_type))
    }

    /// List all registered tenant IDs.
    pub fn tenant_ids(&self) -> Vec<&TenantId> {
        self.tenants.keys().collect()
    }

    /// List all entity types for a tenant.
    pub fn entity_types(&self, tenant: &TenantId) -> Vec<&str> {
        self.tenants
            .get(tenant)
            .map(|tc| tc.entities.keys().map(|k| k.as_str()).collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use temper_spec::csdl::parse_csdl;

    const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
    const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

    fn minimal_csdl() -> (CsdlDocument, String) {
        let doc = parse_csdl(CSDL_XML).expect("CSDL should parse");
        (doc, CSDL_XML.to_string())
    }

    #[test]
    fn register_and_lookup_tenant() {
        let mut registry = SpecRegistry::new();
        let (csdl, csdl_xml) = minimal_csdl();

        registry.register_tenant("ecommerce", csdl, csdl_xml, &[("Order", ORDER_IOA)]);

        let tenant = TenantId::new("ecommerce");
        assert!(registry.get_tenant(&tenant).is_some());
        assert!(registry.get_table(&tenant, "Order").is_some());
        assert!(registry.get_table(&tenant, "NonExistent").is_none());
    }

    #[test]
    fn unknown_tenant_returns_none() {
        let registry = SpecRegistry::new();
        let tenant = TenantId::new("unknown");
        assert!(registry.get_tenant(&tenant).is_none());
        assert!(registry.get_table(&tenant, "Order").is_none());
    }

    #[test]
    fn multiple_tenants_isolated() {
        let mut registry = SpecRegistry::new();
        let (csdl1, csdl_xml1) = minimal_csdl();
        let (csdl2, csdl_xml2) = minimal_csdl();

        registry.register_tenant("ecommerce", csdl1, csdl_xml1, &[("Order", ORDER_IOA)]);
        registry.register_tenant("linear", csdl2, csdl_xml2, &[("Issue", ORDER_IOA)]);

        let ecom = TenantId::new("ecommerce");
        let linear = TenantId::new("linear");

        // Each tenant sees only its own entities
        assert!(registry.get_table(&ecom, "Order").is_some());
        assert!(registry.get_table(&ecom, "Issue").is_none());
        assert!(registry.get_table(&linear, "Issue").is_some());
        assert!(registry.get_table(&linear, "Order").is_none());
    }

    #[test]
    fn tenant_ids_listed() {
        let mut registry = SpecRegistry::new();
        let (csdl1, xml1) = minimal_csdl();
        let (csdl2, xml2) = minimal_csdl();

        registry.register_tenant("alpha", csdl1, xml1, &[]);
        registry.register_tenant("beta", csdl2, xml2, &[]);

        let ids: Vec<&str> = registry.tenant_ids().iter().map(|t| t.as_str()).collect();
        assert!(ids.contains(&"alpha"));
        assert!(ids.contains(&"beta"));
    }

    #[test]
    fn entity_types_for_tenant() {
        let mut registry = SpecRegistry::new();
        let (csdl, xml) = minimal_csdl();

        registry.register_tenant("ecommerce", csdl, xml, &[("Order", ORDER_IOA)]);

        let types = registry.entity_types(&TenantId::new("ecommerce"));
        assert_eq!(types, vec!["Order"]);
    }

    #[test]
    fn transition_table_is_functional() {
        let mut registry = SpecRegistry::new();
        let (csdl, xml) = minimal_csdl();

        registry.register_tenant("ecommerce", csdl, xml, &[("Order", ORDER_IOA)]);

        let table = registry
            .get_table(&TenantId::new("ecommerce"), "Order")
            .unwrap();
        assert_eq!(table.entity_name, "Order");
        assert_eq!(table.initial_state, "Draft");
        assert!(!table.rules.is_empty());

        // Verify it evaluates correctly
        let result = table.evaluate("Draft", 1, "SubmitOrder");
        assert!(result.is_some());
        assert!(result.unwrap().success);
    }

    #[test]
    fn spec_metadata_accessible() {
        let mut registry = SpecRegistry::new();
        let (csdl, xml) = minimal_csdl();

        registry.register_tenant("ecommerce", csdl, xml, &[("Order", ORDER_IOA)]);

        let spec = registry
            .get_spec(&TenantId::new("ecommerce"), "Order")
            .unwrap();
        assert_eq!(spec.automaton.automaton.name, "Order");
        assert!(!spec.ioa_source.is_empty());
    }
}
