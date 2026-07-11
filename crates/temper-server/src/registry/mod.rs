//! Per-tenant specification registry.
//!
//! The [`SpecRegistry`] maps `(TenantId, EntityType)` to parsed specifications
//! and transition tables. It replaces the flat `BTreeMap<String, TransitionTable>` // determinism-ok
//! in `ServerState`, enabling multi-tenant deployments where each tenant has
//! its own entity types and specs.

mod access;
mod relations;
pub mod types;

use std::collections::BTreeMap;
use std::sync::Arc;

use tracing::instrument;

use temper_jit::swap::SwapController;
use temper_jit::table::TransitionTable;
use temper_runtime::tenant::TenantId;
use temper_spec::automaton;
use temper_spec::cross_invariant::parse_cross_invariants;
use temper_spec::csdl::{CsdlDocument, emit_csdl_xml, merge_csdl};

use crate::trigger::types::ReactionRule;

pub use types::*;

use relations::{build_relation_graph, build_webhook_routes};

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
    restore_health: RegistryRestoreHealth,
}

impl SpecRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Health from the current process's durable registry restore.
    pub fn restore_health(&self) -> &RegistryRestoreHealth {
        &self.restore_health
    }

    /// Merge one backend's restore result into current boot health.
    pub(crate) fn record_restore_health(&mut self, report: &RegistryRestoreHealth) {
        self.restore_health.restored_specs = self
            .restore_health
            .restored_specs
            .saturating_add(report.restored_specs);
        for (tenant, quarantine) in &report.quarantined_tenants {
            self.restore_health
                .quarantined_tenants
                .entry(tenant.clone())
                .or_default()
                .entity_failures
                .extend(quarantine.entity_failures.clone());
        }
    }

    /// Replace one tenant's process-local quarantine after its durable tenant
    /// snapshot has been replaced. Retry reconciliation is snapshot-based, so
    /// merging here would retain entities that are no longer active durably.
    pub(crate) fn replace_tenant_restore_quarantine(
        &mut self,
        tenant: &str,
        quarantine: Option<RegistryTenantQuarantine>,
    ) {
        match quarantine {
            Some(quarantine) if !quarantine.entity_failures.is_empty() => {
                self.restore_health
                    .quarantined_tenants
                    .insert(tenant.to_string(), quarantine);
            }
            _ => {
                self.restore_health.quarantined_tenants.remove(tenant);
            }
        }
    }

    /// Return one exact process-local quarantine source identity.
    pub(crate) fn restore_quarantine_identity(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Option<(i64, Option<i64>)> {
        self.restore_health
            .quarantined_tenants
            .get(tenant)
            .and_then(|quarantine| quarantine.entity_failures.get(entity_type))
            .map(|failure| (failure.spec_version, failure.constraint_version))
    }

    /// Reconcile one acknowledged durable record into process-local health.
    ///
    /// This is an optimistic local compare-and-set. The caller snapshots the
    /// complete local identity before any storage await. Missing or stale local
    /// state is reconciled only if it did not change while the durable request
    /// was in flight; spec and constraint versions are never conflated into an
    /// unsound total ordering.
    pub(crate) fn reconcile_acknowledged_restore_quarantine(
        &mut self,
        tenant: &str,
        entity_type: &str,
        expected_local_identity: Option<(i64, Option<i64>)>,
        durable: RegistryQuarantineFailure,
    ) -> bool {
        if self.restore_quarantine_identity(tenant, entity_type) != expected_local_identity {
            return false;
        }
        let failures = &mut self
            .restore_health
            .quarantined_tenants
            .entry(tenant.to_string())
            .or_default()
            .entity_failures;
        failures.insert(entity_type.to_string(), durable);
        true
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

        // Compile every IOA before touching an existing tenant. Restore and
        // hot-reload are transactional at the registry boundary: one malformed
        // entity must not replace CSDL, reactions, or earlier entity tables.
        let compiled_entities = ioa_sources
            .iter()
            .map(|(entity_type, ioa_source)| {
                let automaton = automaton::parse_automaton(ioa_source).map_err(|error| {
                    RegistryError::IoaParse {
                        tenant: tenant_name.clone(),
                        entity_type: (*entity_type).to_string(),
                        source: error.to_string(),
                    }
                })?;
                let table = TransitionTable::from_automaton(&automaton);
                let integrations = automaton.integrations.clone();
                Ok((
                    (*entity_type).to_string(),
                    automaton,
                    table,
                    integrations,
                    (*ioa_source).to_string(),
                ))
            })
            .collect::<Result<Vec<_>, RegistryError>>()?;

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
            existing_config.relation_graph = relation_graph;
            // In merge mode, an incoming payload without cross-invariants must
            // not wipe the ones previously loaded for the tenant — otherwise a
            // follow-up merge (e.g. Agent OS app bootstrap) silently disables
            // user-loaded enforcement. In replace mode, the caller is the new
            // source of truth and the overwrite is intentional.
            if !merge || cross_invariants.is_some() {
                existing_config.cross_invariants = cross_invariants;
                existing_config.cross_invariants_source = cross_invariants_source;
            }

            for (entity_type, automaton, table, integrations, ioa_source) in compiled_entities {
                if let Some(existing_spec) = existing_config.entities.get_mut(&entity_type) {
                    // Hot-swap: write new table into the SAME RwLock that actors hold.
                    let result = existing_spec.swap_controller().swap(table);
                    tracing::info!(
                        %entity_type,
                        ?result,
                        "hot-swapped transition table for existing entity"
                    );
                    // Update metadata on the existing spec.
                    existing_spec.automaton = automaton;
                    existing_spec.integrations = integrations;
                    existing_spec.ioa_source = ioa_source;
                } else {
                    // New entity type — create fresh EntitySpec.
                    existing_config.entities.insert(
                        entity_type,
                        EntitySpec {
                            automaton,
                            integrations,
                            swap: Arc::new(SwapController::new(table)),
                            ioa_source,
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
            for (entity_type, automaton, table, integrations, ioa_source) in compiled_entities {
                entities.insert(
                    entity_type,
                    EntitySpec {
                        automaton,
                        integrations,
                        swap: Arc::new(SwapController::new(table)),
                        ioa_source,
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
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
