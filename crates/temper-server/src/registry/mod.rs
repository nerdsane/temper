//! Per-tenant specification registry.
//!
//! The [`SpecRegistry`] maps `(TenantId, EntityType)` to parsed specifications
//! and transition tables. It replaces the flat `BTreeMap<String, TransitionTable>` // determinism-ok
//! in `ServerState`, enabling multi-tenant deployments where each tenant has
//! its own entity types and specs.

mod registration;
mod relations;
mod server_derived_names;
mod system_one;
pub mod types;

use server_derived_names::warn_on_server_derived_csdl_properties;

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use tracing::instrument;

use temper_jit::swap::SwapController;
use temper_jit::table::TransitionTable;
use temper_runtime::tenant::TenantId;
use temper_spec::FieldInvariant;
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
        // Validate and compile all declarations before touching the live tenant.
        // A bad binding during hot reload must not leave partially changed CSDL
        // or already-swapped transition tables behind.
        let effective_csdl = if merge {
            self.tenants
                .get(&tenant)
                .map(|existing| merge_csdl(&existing.csdl, &csdl))
        } else {
            None
        };
        let automata = system_one::prepare_automata(
            &tenant_name,
            effective_csdl.as_ref().unwrap_or(&csdl),
            ioa_sources,
        )?;
        if merge && let Some(existing) = self.tenants.get(&tenant) {
            system_one::validate_retained_automata(
                &tenant_name,
                effective_csdl.as_ref().unwrap_or(&csdl),
                &existing.entities,
                ioa_sources,
            )?;
        }
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
        warn_on_server_derived_csdl_properties(&tenant_name, &csdl);

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
            existing_config.reactions = if merge {
                merge_reaction_rules(&existing_config.reactions, reactions)
            } else {
                reactions
            };
            // In merge mode, an incoming payload without cross-invariants must
            // not wipe the ones previously loaded for the tenant — otherwise a
            // follow-up merge (e.g. Agent OS app bootstrap) silently disables
            // user-loaded enforcement. In replace mode, the caller is the new
            // source of truth and the overwrite is intentional.
            if !merge || cross_invariants.is_some() {
                existing_config.cross_invariants = cross_invariants;
                existing_config.cross_invariants_source = cross_invariants_source;
            }
            // Rebuild the relation graph. In merge mode it MUST be derived from
            // the MERGED csdl and the effective (post-merge) cross-invariants,
            // not from the incoming csdl alone: a merge of an identity-only
            // CSDL (no navigation relations) would otherwise wipe the tenant's
            // full relation graph — breaking FK validation, restricted deletes,
            // and $expand for the process lifetime. In replace mode the caller
            // is the new source of truth, so the incoming graph stands.
            let rebuilt_relation_graph = if merge {
                build_relation_graph(
                    &existing_config.csdl,
                    existing_config.cross_invariants.as_ref(),
                )
            } else {
                relation_graph
            };
            existing_config.relation_graph = rebuilt_relation_graph;

            for ((entity_type, ioa_source), automaton) in ioa_sources.iter().zip(automata) {
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
            for ((entity_type, ioa_source), automaton) in ioa_sources.iter().zip(automata) {
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

    /// Build a [`ReactionRegistry`] from all tenants' reaction rules,
    /// including synthesized rules from `[[agent_trigger]]` sections.
    pub fn build_reaction_registry(&self) -> ReactionRegistry {
        let mut registry = ReactionRegistry::new();
        for (tenant, config) in &self.tenants {
            let mut rules = config.reactions.clone();
            // ADR-0046: synthesize reaction rules from [[action.triggers]]
            // entity-kind blocks on every entity's actions. Wasm/Webhook
            // kinds are handled by a separate runtime path.
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
