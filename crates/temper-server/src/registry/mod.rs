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

use temper_jit::swap::{SwapController, SwapResult};
use temper_jit::table::TransitionTable;
use temper_runtime::tenant::TenantId;
use temper_spec::FieldInvariant;
use temper_spec::automaton;
use temper_spec::cross_invariant::parse_cross_invariants;
use temper_spec::csdl::{CsdlDocument, emit_csdl_xml, merge_csdl};

use crate::trigger::ReactionRegistry;
use crate::trigger::types::ReactionRule;

pub use types::*;

use relations::{build_relation_graph, build_webhook_routes, synthesize_action_trigger_reaction};

fn merge_reaction_rules(
    existing: &[ReactionRule],
    incoming: Vec<ReactionRule>,
) -> Vec<ReactionRule> {
    let mut merged: BTreeMap<String, ReactionRule> = existing
        .iter()
        .cloned()
        .map(|rule| (rule.name.clone(), rule))
        .collect();
    for rule in incoming {
        merged.insert(rule.name.clone(), rule);
    }
    merged.into_values().collect()
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
}

mod registration;

impl SpecRegistry {
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

    /// Subscribe to transition-table version changes for an entity type.
    pub(crate) fn subscribe_table_versions(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Option<tokio::sync::watch::Receiver<u64>> {
        self.tenants
            .get(tenant)
            .and_then(|tc| tc.entities.get(entity_type))
            .map(EntitySpec::subscribe_table_versions)
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

    /// Look up the `[[field_invariant]]` declarations for a tenant and entity
    /// type, returning a cloned snapshot so the caller does not need to hold a
    /// registry read lock across subsequent async work.
    pub fn field_invariants_for(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Option<Vec<FieldInvariant>> {
        self.tenants
            .get(tenant)
            .and_then(|tc| tc.entities.get(entity_type))
            .map(|es| es.automaton.field_invariants.clone())
    }

    /// Mutable access to the IOA spec for a tenant and entity type.
    pub fn get_spec_mut(
        &mut self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Option<&mut EntitySpec> {
        self.tenants
            .get_mut(tenant)
            .and_then(|tc| tc.entities.get_mut(entity_type))
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
mod tests;
