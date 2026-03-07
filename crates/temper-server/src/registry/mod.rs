//! Per-tenant specification registry.
//!
//! The [`SpecRegistry`] maps `(TenantId, EntityType)` to parsed specifications
//! and transition tables. It replaces the flat `BTreeMap<String, TransitionTable>` // determinism-ok
//! in `ServerState`, enabling multi-tenant deployments where each tenant has
//! its own entity types and specs.

mod relations;
pub mod types;

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use tracing::instrument;

use temper_jit::swap::SwapController;
use temper_jit::table::TransitionTable;
use temper_runtime::tenant::TenantId;
use temper_spec::automaton;
use temper_spec::cross_invariant::parse_cross_invariants;
use temper_spec::csdl::{CsdlDocument, emit_csdl_xml, merge_csdl};

use crate::reaction::ReactionRegistry;
use crate::reaction::types::ReactionRule;

pub use types::*;

use relations::{build_relation_graph, build_webhook_routes, synthesize_agent_trigger_reactions};

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
    ///
    /// If the tenant already exists, existing entity tables are hot-swapped
    /// via their [`SwapController`] so that live actors see the new table on
    /// their next action dispatch — no restart required. New entities are
    /// added; entities not in the new spec set are removed.
    pub fn register_tenant(
        &mut self,
        tenant: impl Into<TenantId>,
        csdl: CsdlDocument,
        csdl_xml: String,
        ioa_sources: &[(&str, &str)],
    ) {
        self.try_register_tenant_with_reactions_and_constraints(
            tenant,
            csdl,
            csdl_xml,
            ioa_sources,
            Vec::new(),
            None,
            false,
        )
        .unwrap_or_else(|e| panic!("{e}"));
    }

    /// Fallible variant of [`register_tenant`](Self::register_tenant).
    pub fn try_register_tenant(
        &mut self,
        tenant: impl Into<TenantId>,
        csdl: CsdlDocument,
        csdl_xml: String,
        ioa_sources: &[(&str, &str)],
    ) -> Result<(), RegistryError> {
        self.try_register_tenant_with_reactions_and_constraints(
            tenant,
            csdl,
            csdl_xml,
            ioa_sources,
            Vec::new(),
            None,
            false,
        )
    }

    /// Register a tenant with CSDL, IOA specs, reaction rules, and optional
    /// cross-entity invariant definitions.
    pub fn register_tenant_with_reactions_and_constraints(
        &mut self,
        tenant: impl Into<TenantId>,
        csdl: CsdlDocument,
        csdl_xml: String,
        ioa_sources: &[(&str, &str)],
        reactions: Vec<ReactionRule>,
        cross_invariants_source: Option<String>,
    ) {
        self.try_register_tenant_with_reactions_and_constraints(
            tenant,
            csdl,
            csdl_xml,
            ioa_sources,
            reactions,
            cross_invariants_source,
            false,
        )
        .unwrap_or_else(|e| panic!("{e}"));
    }

    /// Fallible variant of [`register_tenant_with_reactions_and_constraints`](Self::register_tenant_with_reactions_and_constraints).
    ///
    /// When `merge` is `true`, the new specs are **merged** into the existing
    /// tenant config rather than replacing it.  Existing entity types, CSDL
    /// schemas, and entity-set-map entries that are not part of the new
    /// submission are preserved.  This is the correct mode for
    /// `load-inline` (agent `submit_specs`), where the agent only submits
    /// its own entities and should not wipe platform types.
    #[allow(clippy::too_many_arguments)]
    #[instrument(skip_all, fields(otel.name = "registry.try_register_tenant_with_reactions_and_constraints"))]
    pub fn try_register_tenant_with_reactions_and_constraints(
        &mut self,
        tenant: impl Into<TenantId>,
        csdl: CsdlDocument,
        csdl_xml: String,
        ioa_sources: &[(&str, &str)],
        reactions: Vec<ReactionRule>,
        cross_invariants_source: Option<String>,
        merge: bool,
    ) -> Result<(), RegistryError> {
        let tenant = tenant.into();
        let tenant_name = tenant.to_string();
        let cross_invariants = cross_invariants_source
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .map(|s| {
                parse_cross_invariants(s).map_err(|e| RegistryError::CrossInvariantParse {
                    tenant: tenant_name.clone(),
                    source: e.to_string(),
                })
            })
            .transpose()?;
        let relation_graph = build_relation_graph(&csdl, cross_invariants.as_ref());

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

        if let Some(existing_config) = self.tenants.get_mut(&tenant) {
            // Hot-reload path: swap tables on existing entities, add new ones.
            if merge {
                // Merge mode: combine incoming CSDL/entity-set-map with existing.
                let merged_csdl = merge_csdl(&existing_config.csdl, &csdl);
                existing_config.csdl_xml = Arc::new(emit_csdl_xml(&merged_csdl));
                existing_config.csdl = Arc::new(merged_csdl);
                for (k, v) in entity_set_map {
                    existing_config.entity_set_map.insert(k, v);
                }
            } else {
                // Replace mode: full replacement (for load-dir where directory is truth).
                existing_config.csdl = Arc::new(csdl);
                existing_config.csdl_xml = Arc::new(csdl_xml);
                existing_config.entity_set_map = entity_set_map;
            }
            existing_config.reactions = reactions;
            existing_config.relation_graph = relation_graph;
            existing_config.cross_invariants = cross_invariants;
            existing_config.cross_invariants_source = cross_invariants_source;

            for (entity_type, ioa_source) in ioa_sources {
                let automaton = automaton::parse_automaton(ioa_source).map_err(|e| {
                    RegistryError::IoaParse {
                        tenant: tenant_name.clone(),
                        entity_type: (*entity_type).to_string(),
                        source: e.to_string(),
                    }
                })?;
                let table = TransitionTable::from_automaton(&automaton);
                let integrations = automaton.integrations.clone();

                if let Some(existing_spec) = existing_config.entities.get_mut(*entity_type) {
                    // Hot-swap: write new table into the SAME RwLock that actors hold.
                    let result = existing_spec.swap_controller().swap(table);
                    tracing::info!(
                        entity_type,
                        ?result,
                        "hot-swapped transition table for existing entity"
                    );
                    // Update metadata on the existing spec.
                    existing_spec.automaton = automaton;
                    existing_spec.integrations = integrations;
                    existing_spec.ioa_source = ioa_source.to_string();
                } else {
                    // New entity type — create fresh EntitySpec.
                    existing_config.entities.insert(
                        entity_type.to_string(),
                        EntitySpec {
                            automaton,
                            integrations,
                            swap: Arc::new(SwapController::new(table)),
                            ioa_source: ioa_source.to_string(),
                        },
                    );
                }
            }

            if !merge {
                // Replace mode: remove entities no longer in the spec set.
                let new_entity_types: std::collections::BTreeSet<String> =
                    ioa_sources.iter().map(|(t, _)| t.to_string()).collect();
                existing_config
                    .entities
                    .retain(|k, _| new_entity_types.contains(k));
            }

            // Rebuild webhook route index.
            existing_config.webhook_routes = build_webhook_routes(&existing_config.entities);

            if merge {
                // Merge mode: only reset verification for entities in this submission.
                for (entity_type, _) in ioa_sources {
                    existing_config
                        .verification
                        .insert(entity_type.to_string(), VerificationStatus::Pending);
                }
            } else {
                // Replace mode: reset verification for all entities.
                existing_config.verification = existing_config
                    .entities
                    .keys()
                    .map(|k| (k.clone(), VerificationStatus::Pending))
                    .collect();
            }
        } else {
            // First registration: create new TenantConfig.
            let mut entities = BTreeMap::new();
            for (entity_type, ioa_source) in ioa_sources {
                let automaton = automaton::parse_automaton(ioa_source).map_err(|e| {
                    RegistryError::IoaParse {
                        tenant: tenant_name.clone(),
                        entity_type: (*entity_type).to_string(),
                        source: e.to_string(),
                    }
                })?;
                let table = TransitionTable::from_automaton(&automaton);
                let integrations = automaton.integrations.clone();
                entities.insert(
                    entity_type.to_string(),
                    EntitySpec {
                        automaton,
                        integrations,
                        swap: Arc::new(SwapController::new(table)),
                        ioa_source: ioa_source.to_string(),
                    },
                );
            }

            let verification = entities
                .keys()
                .map(|k| (k.clone(), VerificationStatus::Pending))
                .collect();

            let webhook_routes = build_webhook_routes(&entities);
            self.tenants.insert(
                tenant,
                TenantConfig {
                    csdl: Arc::new(csdl),
                    csdl_xml: Arc::new(csdl_xml),
                    entity_set_map,
                    entities,
                    reactions,
                    relation_graph,
                    cross_invariants,
                    cross_invariants_source,
                    webhook_routes,
                    verification,
                },
            );
        }

        Ok(())
    }

    /// Register a tenant with CSDL, IOA specs, and reaction rules.
    pub fn register_tenant_with_reactions(
        &mut self,
        tenant: impl Into<TenantId>,
        csdl: CsdlDocument,
        csdl_xml: String,
        ioa_sources: &[(&str, &str)],
        reactions: Vec<ReactionRule>,
    ) {
        self.try_register_tenant_with_reactions_and_constraints(
            tenant,
            csdl,
            csdl_xml,
            ioa_sources,
            reactions,
            None,
            false,
        )
        .unwrap_or_else(|e| panic!("{e}"));
    }

    /// Fallible variant of [`register_tenant_with_reactions`](Self::register_tenant_with_reactions).
    pub fn try_register_tenant_with_reactions(
        &mut self,
        tenant: impl Into<TenantId>,
        csdl: CsdlDocument,
        csdl_xml: String,
        ioa_sources: &[(&str, &str)],
        reactions: Vec<ReactionRule>,
    ) -> Result<(), RegistryError> {
        self.try_register_tenant_with_reactions_and_constraints(
            tenant,
            csdl,
            csdl_xml,
            ioa_sources,
            reactions,
            None,
            false,
        )
    }

    /// Build a [`ReactionRegistry`] from all tenants' reaction rules,
    /// including synthesized rules from `[[agent_trigger]]` sections.
    pub fn build_reaction_registry(&self) -> ReactionRegistry {
        let mut registry = ReactionRegistry::new();
        for (tenant, config) in &self.tenants {
            let mut rules = config.reactions.clone();
            // Synthesize reaction rules from agent triggers in each entity spec.
            for (entity_type, spec) in &config.entities {
                for trigger in &spec.automaton.agent_triggers {
                    rules.extend(synthesize_agent_trigger_reactions(entity_type, trigger));
                }
            }
            if !rules.is_empty() {
                registry.register_tenant_rules(tenant.clone(), rules);
            }
        }
        registry
    }

    /// Look up a tenant's configuration.
    pub fn get_tenant(&self, tenant: &TenantId) -> Option<&TenantConfig> {
        self.tenants.get(tenant)
    }

    /// Look up a transition table for a specific tenant and entity type.
    ///
    /// Returns a snapshot of the current table. If a hot-swap has occurred
    /// since the last call, this returns the new table.
    pub fn get_table(&self, tenant: &TenantId, entity_type: &str) -> Option<Arc<TransitionTable>> {
        self.tenants
            .get(tenant)
            .and_then(|tc| tc.entities.get(entity_type))
            .map(|es| es.table())
    }

    /// Get a live reference to the transition table's `RwLock`.
    ///
    /// Unlike [`get_table()`](Self::get_table) which returns a cloned snapshot,
    /// this returns the `Arc<RwLock<TransitionTable>>` from the [`SwapController`].
    /// Actors holding this reference will see hot-swapped tables on their next read.
    pub fn get_table_live(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Option<Arc<RwLock<TransitionTable>>> {
        self.tenants
            .get(tenant)
            .and_then(|tc| tc.entities.get(entity_type))
            .map(|es| es.swap_controller().current())
    }

    /// Look up the entity type name for an entity set in a tenant.
    pub fn resolve_entity_type(&self, tenant: &TenantId, entity_set: &str) -> Option<String> {
        self.tenants
            .get(tenant)
            .and_then(|tc| tc.entity_set_map.get(entity_set).cloned())
    }

    /// Look up the IOA spec for a tenant and entity type.
    pub fn get_spec(&self, tenant: &TenantId, entity_type: &str) -> Option<&EntitySpec> {
        self.tenants
            .get(tenant)
            .and_then(|tc| tc.entities.get(entity_type))
    }

    /// Remove a tenant and all its specs from the registry.
    ///
    /// Returns `true` if the tenant was found and removed, `false` otherwise.
    #[instrument(skip_all, fields(otel.name = "registry.remove_tenant", tenant = %tenant))]
    pub fn remove_tenant(&mut self, tenant: &TenantId) -> bool {
        self.tenants.remove(tenant).is_some()
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

    /// Set verification status for a specific entity type.
    #[instrument(skip_all, fields(otel.name = "registry.set_verification_status", tenant = %tenant, entity_type))]
    pub fn set_verification_status(
        &mut self,
        tenant: &TenantId,
        entity_type: &str,
        status: VerificationStatus,
    ) {
        if let Some(config) = self.tenants.get_mut(tenant) {
            config.verification.insert(entity_type.to_string(), status);
        }
    }

    /// Get verification status for a specific entity type.
    pub fn get_verification_status(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Option<&VerificationStatus> {
        self.tenants
            .get(tenant)
            .and_then(|tc| tc.verification.get(entity_type))
    }

    /// Get all verification statuses for a tenant.
    pub fn verification_statuses(
        &self,
        tenant: &TenantId,
    ) -> Option<&BTreeMap<String, VerificationStatus>> {
        self.tenants.get(tenant).map(|tc| &tc.verification)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use temper_spec::csdl::parse_csdl;

    const CSDL_XML: &str = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
    const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

    fn minimal_csdl() -> (CsdlDocument, String) {
        let doc = parse_csdl(CSDL_XML).expect("CSDL should parse");
        (doc, CSDL_XML.to_string())
    }

    #[test]
    fn register_and_lookup_tenant() {
        let mut registry = SpecRegistry::new();
        let (csdl, csdl_xml) = minimal_csdl();

        registry.register_tenant("alpha", csdl, csdl_xml, &[("Order", ORDER_IOA)]);

        let tenant = TenantId::new("alpha");
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

        registry.register_tenant("alpha", csdl1, csdl_xml1, &[("Order", ORDER_IOA)]);
        registry.register_tenant("beta", csdl2, csdl_xml2, &[("Task", ORDER_IOA)]);

        let a = TenantId::new("alpha");
        let b = TenantId::new("beta");

        // Each tenant sees only its own entities
        assert!(registry.get_table(&a, "Order").is_some());
        assert!(registry.get_table(&a, "Task").is_none());
        assert!(registry.get_table(&b, "Task").is_some());
        assert!(registry.get_table(&b, "Order").is_none());
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

        registry.register_tenant("alpha", csdl, xml, &[("Order", ORDER_IOA)]);

        let types = registry.entity_types(&TenantId::new("alpha"));
        assert_eq!(types, vec!["Order"]);
    }

    #[test]
    fn transition_table_is_functional() {
        let mut registry = SpecRegistry::new();
        let (csdl, xml) = minimal_csdl();

        registry.register_tenant("alpha", csdl, xml, &[("Order", ORDER_IOA)]);

        let table = registry
            .get_table(&TenantId::new("alpha"), "Order")
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
    fn remove_tenant_succeeds() {
        let mut registry = SpecRegistry::new();
        let (csdl, xml) = minimal_csdl();

        registry.register_tenant("doomed", csdl, xml, &[("Order", ORDER_IOA)]);
        let tenant = TenantId::new("doomed");
        assert!(registry.get_tenant(&tenant).is_some());

        assert!(registry.remove_tenant(&tenant));
        assert!(registry.get_tenant(&tenant).is_none());
        assert!(registry.get_table(&tenant, "Order").is_none());
    }

    #[test]
    fn remove_nonexistent_tenant_returns_false() {
        let mut registry = SpecRegistry::new();
        let tenant = TenantId::new("nonexistent");
        assert!(!registry.remove_tenant(&tenant));
    }

    #[test]
    fn spec_metadata_accessible() {
        let mut registry = SpecRegistry::new();
        let (csdl, xml) = minimal_csdl();

        registry.register_tenant("alpha", csdl, xml, &[("Order", ORDER_IOA)]);

        let spec = registry.get_spec(&TenantId::new("alpha"), "Order").unwrap();
        assert_eq!(spec.automaton.automaton.name, "Order");
        assert!(!spec.ioa_source.is_empty());
    }

    /// Minimal CSDL with a single EntityType + EntitySet for merge tests.
    fn task_csdl() -> (CsdlDocument, String) {
        let xml = r#"<?xml version="1.0"?>
        <edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
          <edmx:DataServices>
            <Schema Namespace="Temper.Example" xmlns="http://docs.oasis-open.org/odata/ns/edm">
              <EntityType Name="Task">
                <Key><PropertyRef Name="Id"/></Key>
                <Property Name="Id" Type="Edm.Guid" Nullable="false"/>
              </EntityType>
              <EntityContainer Name="ExampleService">
                <EntitySet Name="Tasks" EntityType="Temper.Example.Task"/>
              </EntityContainer>
            </Schema>
          </edmx:DataServices>
        </edmx:Edmx>"#;
        (parse_csdl(xml).unwrap(), xml.to_string())
    }

    #[test]
    fn merge_preserves_existing_entities_and_entity_set_map() {
        let mut registry = SpecRegistry::new();
        let (csdl, xml) = minimal_csdl();
        registry.register_tenant("alpha", csdl, xml, &[("Order", ORDER_IOA)]);
        let tenant = TenantId::new("alpha");

        let (new_csdl, new_xml) = task_csdl();
        registry
            .try_register_tenant_with_reactions_and_constraints(
                "alpha",
                new_csdl,
                new_xml,
                &[("Task", ORDER_IOA)],
                Vec::new(),
                None,
                true,
            )
            .expect("merge should succeed");

        assert!(
            registry.get_table(&tenant, "Order").is_some(),
            "Order survives merge"
        );
        assert!(
            registry.get_table(&tenant, "Task").is_some(),
            "Task added by merge"
        );

        let config = registry.get_tenant(&tenant).unwrap();
        assert!(config.entity_set_map.contains_key("Orders"));
        assert!(config.entity_set_map.contains_key("Tasks"));
        assert!(matches!(
            config.verification.get("Task"),
            Some(VerificationStatus::Pending)
        ));
    }

    #[test]
    fn replace_removes_entities_not_in_new_spec_set() {
        let mut registry = SpecRegistry::new();
        let (csdl, xml) = minimal_csdl();
        registry.register_tenant("alpha", csdl, xml, &[("Order", ORDER_IOA)]);
        let tenant = TenantId::new("alpha");

        let (csdl2, xml2) = minimal_csdl();
        registry
            .try_register_tenant_with_reactions_and_constraints(
                "alpha",
                csdl2,
                xml2,
                &[("Task", ORDER_IOA)],
                Vec::new(),
                None,
                false,
            )
            .expect("replace should succeed");

        assert!(
            registry.get_table(&tenant, "Order").is_none(),
            "Order removed in replace"
        );
        assert!(
            registry.get_table(&tenant, "Task").is_some(),
            "Task exists after replace"
        );
    }
}
