//! Per-tenant specification registry.
//!
//! The [`SpecRegistry`] maps `(TenantId, EntityType)` to parsed specifications
//! and transition tables. It replaces the flat `BTreeMap<String, TransitionTable>` // determinism-ok
//! in `ServerState`, enabling multi-tenant deployments where each tenant has
//! its own entity types and specs.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use temper_jit::swap::SwapController;
use temper_jit::table::TransitionTable;
use temper_runtime::tenant::TenantId;
use temper_spec::automaton::{self, Automaton, Integration};
use temper_spec::cross_invariant::{CrossInvariantSpec, DeletePolicy, parse_cross_invariants};
use temper_spec::csdl::CsdlDocument;

use crate::reaction::ReactionRegistry;
use crate::reaction::types::ReactionRule;

/// Verification status for a single entity type.
#[derive(Debug, Clone, serde::Serialize)]
pub enum VerificationStatus {
    /// Verification has not started yet.
    Pending,
    /// Verification is currently running.
    Running,
    /// Verification completed with results.
    Completed(EntityVerificationResult),
}

/// Summary of verification results for an entity type.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EntityVerificationResult {
    /// Whether all levels passed.
    pub all_passed: bool,
    /// Per-level summaries.
    pub levels: Vec<EntityLevelSummary>,
    /// ISO-8601 timestamp when verification completed.
    pub verified_at: String,
}

/// Summary of a single verification level.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EntityLevelSummary {
    /// Level name (e.g. "L0 SMT", "L1 Model Check").
    pub level: String,
    /// Whether this level passed.
    pub passed: bool,
    /// Human-readable summary.
    pub summary: String,
    /// Detailed violation information (populated only for failed levels).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Vec<VerificationDetail>>,
}

/// A single verification violation detail.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VerificationDetail {
    /// Violation kind: "liveness_violation", "invariant_violation", "counterexample", "proptest_failure".
    pub kind: String,
    /// Property or invariant name that was violated.
    pub property: String,
    /// Human-readable description of the violation.
    pub description: String,
    /// Actor ID that triggered the violation (if applicable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
}

/// Errors raised while registering tenant specifications.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    /// cross-invariants TOML failed to parse.
    CrossInvariantParse { tenant: String, source: String },
    /// An IOA source failed to parse.
    IoaParse {
        tenant: String,
        entity_type: String,
        source: String,
    },
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CrossInvariantParse { tenant, source } => {
                write!(
                    f,
                    "failed to parse cross-invariants for tenant '{tenant}': {source}"
                )
            }
            Self::IoaParse {
                tenant,
                entity_type,
                source,
            } => {
                write!(
                    f,
                    "failed to parse IOA for tenant '{tenant}', entity '{entity_type}': {source}"
                )
            }
        }
    }
}

impl std::error::Error for RegistryError {}

/// A compiled relation edge from CSDL navigation metadata.
#[derive(Debug, Clone)]
pub struct RelationEdge {
    /// Source entity type that owns the FK field.
    pub from_entity: String,
    /// Navigation property name on the source entity.
    pub navigation_property: String,
    /// Target entity type.
    pub to_entity: String,
    /// FK field on source entity (e.g. `OrderId`).
    pub source_field: String,
    /// Referenced key on target entity (usually `Id`).
    pub target_field: String,
    /// Whether the relationship allows null references.
    pub nullable: bool,
    /// Delete policy applied to this edge.
    pub delete_policy: DeletePolicy,
}

/// Tenant-scoped relation graph compiled from CSDL.
#[derive(Debug, Clone, Default)]
pub struct RelationGraph {
    /// Outgoing edges keyed by source entity type.
    pub outgoing: BTreeMap<String, Vec<RelationEdge>>,
    /// Incoming edges keyed by target entity type.
    pub incoming: BTreeMap<String, Vec<RelationEdge>>,
}

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
    /// Reaction rules for cross-entity coordination.
    pub reactions: Vec<ReactionRule>,
    /// Tenant relation graph compiled from CSDL.
    pub relation_graph: RelationGraph,
    /// Optional parsed cross-entity invariant spec.
    pub cross_invariants: Option<CrossInvariantSpec>,
    /// Raw `cross-invariants.toml` source, if provided.
    pub cross_invariants_source: Option<String>,
    /// Per-entity verification status (design-time observation).
    pub verification: BTreeMap<String, VerificationStatus>,
}

/// A registered entity type's spec and transition table.
///
/// The table is wrapped in a [`SwapController`] to enable atomic hot-swap
/// without restarting actors. Use [`swap_controller()`] to access the
/// controller for hot-swap operations.
#[derive(Clone)]
pub struct EntitySpec {
    /// The parsed I/O Automaton specification.
    pub automaton: Automaton,
    /// Integration declarations from the IOA spec.
    pub integrations: Vec<Integration>,
    /// Hot-swappable transition table controller.
    swap: Arc<SwapController>,
    /// Raw IOA TOML source (for invariant parsing, display, etc.).
    pub ioa_source: String,
}

impl std::fmt::Debug for EntitySpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EntitySpec")
            .field("automaton", &self.automaton)
            .field("version", &self.swap.version())
            .field("ioa_source_len", &self.ioa_source.len())
            .finish()
    }
}

impl EntitySpec {
    /// Get a snapshot of the current transition table.
    ///
    /// This reads through the [`SwapController`] — if a hot-swap happened,
    /// subsequent calls return the new table.
    pub fn table(&self) -> Arc<TransitionTable> {
        let lock = self.swap.current();
        let table = lock.read().expect("SwapController lock poisoned");
        // Clone the table out of the RwLock into an Arc for the caller.
        // This is cheap — TransitionTable is small (a few Vecs of strings).
        Arc::new(table.clone())
    }

    /// Get the [`SwapController`] for hot-swap operations.
    pub fn swap_controller(&self) -> &Arc<SwapController> {
        &self.swap
    }
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
        )
        .unwrap_or_else(|e| panic!("{e}"));
    }

    /// Fallible variant of [`register_tenant_with_reactions_and_constraints`](Self::register_tenant_with_reactions_and_constraints).
    pub fn try_register_tenant_with_reactions_and_constraints(
        &mut self,
        tenant: impl Into<TenantId>,
        csdl: CsdlDocument,
        csdl_xml: String,
        ioa_sources: &[(&str, &str)],
        reactions: Vec<ReactionRule>,
        cross_invariants_source: Option<String>,
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
            existing_config.csdl = Arc::new(csdl);
            existing_config.csdl_xml = Arc::new(csdl_xml);
            existing_config.entity_set_map = entity_set_map;
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

            // Remove entities no longer in the spec set.
            let new_entity_types: std::collections::BTreeSet<String> =
                ioa_sources.iter().map(|(t, _)| t.to_string()).collect();
            existing_config
                .entities
                .retain(|k, _| new_entity_types.contains(k));

            // Reset verification to Pending for re-verification.
            existing_config.verification = existing_config
                .entities
                .keys()
                .map(|k| (k.clone(), VerificationStatus::Pending))
                .collect();
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
        )
    }

    /// Build a [`ReactionRegistry`] from all tenants' reaction rules.
    pub fn build_reaction_registry(&self) -> ReactionRegistry {
        let mut registry = ReactionRegistry::new();
        for (tenant, config) in &self.tenants {
            if !config.reactions.is_empty() {
                registry.register_tenant_rules(tenant.clone(), config.reactions.clone());
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

fn build_relation_graph(
    csdl: &CsdlDocument,
    cross_invariants: Option<&CrossInvariantSpec>,
) -> RelationGraph {
    let mut overrides = BTreeMap::<(String, String), DeletePolicy>::new();
    let default_policy = cross_invariants
        .map(|spec| {
            for ov in &spec.relation_overrides {
                overrides.insert(
                    (ov.from_entity.clone(), ov.navigation_property.clone()),
                    ov.delete_policy,
                );
            }
            spec.default_delete_policy
        })
        .unwrap_or(DeletePolicy::Restrict);

    let mut graph = RelationGraph::default();
    for schema in &csdl.schemas {
        for et in &schema.entity_types {
            for nav in &et.navigation_properties {
                let target = nav_target_entity(&nav.type_name);
                for rc in &nav.referential_constraints {
                    let delete_policy = overrides
                        .get(&(et.name.clone(), nav.name.clone()))
                        .copied()
                        .unwrap_or(default_policy);
                    let edge = RelationEdge {
                        from_entity: et.name.clone(),
                        navigation_property: nav.name.clone(),
                        to_entity: target.clone(),
                        source_field: rc.property.clone(),
                        target_field: rc.referenced_property.clone(),
                        nullable: nav.nullable,
                        delete_policy,
                    };
                    graph
                        .outgoing
                        .entry(et.name.clone())
                        .or_default()
                        .push(edge.clone());
                    graph.incoming.entry(target.clone()).or_default().push(edge);
                }
            }
        }
    }
    graph
}

fn nav_target_entity(type_name: &str) -> String {
    let raw = type_name.trim();
    let inner = if raw.starts_with("Collection(") && raw.ends_with(')') {
        &raw[11..raw.len() - 1]
    } else {
        raw
    };
    inner.rsplit('.').next().unwrap_or(inner).to_string()
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
}
