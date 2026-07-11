//! Registry lookup, status, and reaction-index accessors.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use temper_jit::table::TransitionTable;
use temper_runtime::tenant::TenantId;
use temper_spec::FieldInvariant;
use tracing::instrument;

use super::relations::synthesize_action_trigger_reaction;
use super::{EntitySpec, SpecRegistry, TenantConfig, VerificationStatus};
use crate::trigger::ReactionRegistry;

impl SpecRegistry {
    /// Snapshot one tenant's process-local registry sources.
    pub(crate) fn tenant_source_snapshot(
        &self,
        tenant: &TenantId,
    ) -> Option<super::RegistryTenantSourceSnapshot> {
        self.tenants
            .get(tenant)
            .map(|config| super::RegistryTenantSourceSnapshot {
                csdl_xml: config.csdl_xml.as_ref().clone(),
                ioa_sources: config
                    .entities
                    .iter()
                    .map(|(entity_type, spec)| (entity_type.clone(), spec.ioa_source.clone()))
                    .collect(),
                cross_invariants_source: config.cross_invariants_source.clone(),
            })
    }

    /// Build a [`ReactionRegistry`] from every tenant's explicit and synthesized rules.
    pub fn build_reaction_registry(&self) -> ReactionRegistry {
        let mut registry = ReactionRegistry::new();
        for (tenant, config) in &self.tenants {
            let mut rules = config.reactions.clone();
            for (entity_type, spec) in &config.entities {
                for action in &spec.automaton.actions {
                    for trigger in &action.triggers {
                        if let Some(rule) =
                            synthesize_action_trigger_reaction(entity_type, &action.name, trigger)
                        {
                            rules.push(rule);
                        }
                    }
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

    /// Return a snapshot of one tenant entity's current transition table.
    pub fn get_table(&self, tenant: &TenantId, entity_type: &str) -> Option<Arc<TransitionTable>> {
        self.tenants
            .get(tenant)
            .and_then(|config| config.entities.get(entity_type))
            .map(EntitySpec::table)
    }

    /// Return the live transition-table lock used by existing actors.
    pub fn get_table_live(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Option<Arc<RwLock<TransitionTable>>> {
        self.tenants
            .get(tenant)
            .and_then(|config| config.entities.get(entity_type))
            .map(|spec| spec.swap_controller().current())
    }

    /// Resolve an OData entity-set name to its entity type.
    pub fn resolve_entity_type(&self, tenant: &TenantId, entity_set: &str) -> Option<String> {
        self.tenants
            .get(tenant)
            .and_then(|config| config.entity_set_map.get(entity_set).cloned())
    }

    /// Look up one tenant entity's parsed IOA specification.
    pub fn get_spec(&self, tenant: &TenantId, entity_type: &str) -> Option<&EntitySpec> {
        self.tenants
            .get(tenant)
            .and_then(|config| config.entities.get(entity_type))
    }

    /// Clone field invariants without holding the registry lock across async work.
    pub fn field_invariants_for(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Option<Vec<FieldInvariant>> {
        self.get_spec(tenant, entity_type)
            .map(|spec| spec.automaton.field_invariants.clone())
    }

    /// Mutably access one tenant entity's parsed IOA specification.
    pub fn get_spec_mut(
        &mut self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Option<&mut EntitySpec> {
        self.tenants
            .get_mut(tenant)
            .and_then(|config| config.entities.get_mut(entity_type))
    }

    /// Remove a tenant and every registered specification it owns.
    #[instrument(skip_all, fields(otel.name = "registry.remove_tenant", tenant = %tenant))]
    pub fn remove_tenant(&mut self, tenant: &TenantId) -> bool {
        self.tenants.remove(tenant).is_some()
    }

    /// List registered tenant IDs in deterministic order.
    pub fn tenant_ids(&self) -> Vec<&TenantId> {
        self.tenants.keys().collect()
    }

    /// List one tenant's entity types in deterministic order.
    pub fn entity_types(&self, tenant: &TenantId) -> Vec<&str> {
        self.tenants
            .get(tenant)
            .map(|config| config.entities.keys().map(String::as_str).collect())
            .unwrap_or_default()
    }

    /// Set verification status for one tenant entity type.
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

    /// Get verification status for one tenant entity type.
    pub fn get_verification_status(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Option<&VerificationStatus> {
        self.tenants
            .get(tenant)
            .and_then(|config| config.verification.get(entity_type))
    }

    /// Get all verification statuses for one tenant.
    pub fn verification_statuses(
        &self,
        tenant: &TenantId,
    ) -> Option<&BTreeMap<String, VerificationStatus>> {
        self.tenants.get(tenant).map(|config| &config.verification)
    }
}
