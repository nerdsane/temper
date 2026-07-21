//! Tenant registration and hot-swap operations.

use super::*;

impl SpecRegistry {
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
        let table_change_tx = self.table_change_tx.clone();
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
                        EntitySpec::new(
                            tenant.clone(),
                            entity_type.to_string(),
                            automaton,
                            integrations,
                            table,
                            ioa_source.to_string(),
                            table_change_tx.clone(),
                        ),
                    );
                    let _ = table_change_tx.send(RegistryTableChange {
                        tenant: tenant.clone(),
                        entity_type: entity_type.to_string(),
                        version: 1,
                    });
                }
            }

            if !merge {
                // Replace mode: remove entities no longer in the spec set.
                let new_entity_types: std::collections::BTreeSet<String> =
                    ioa_sources.iter().map(|(t, _)| t.to_string()).collect();
                existing_config.entities.retain(|entity_type, spec| {
                    let retain = new_entity_types.contains(entity_type);
                    if !retain {
                        let removal_version = spec
                            .swap_controller()
                            .version()
                            .checked_add(1)
                            .expect("transition table version overflow");
                        spec.table_version_tx.send_replace(removal_version);
                    }
                    retain
                });
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
                    EntitySpec::new(
                        tenant.clone(),
                        entity_type.to_string(),
                        automaton,
                        integrations,
                        table,
                        ioa_source.to_string(),
                        table_change_tx.clone(),
                    ),
                );
            }

            let verification = entities
                .keys()
                .map(|k| (k.clone(), VerificationStatus::Pending))
                .collect();

            let webhook_routes = build_webhook_routes(&entities);
            let registered_entity_types: Vec<_> = entities.keys().cloned().collect();
            self.tenants.insert(
                tenant.clone(),
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
            for entity_type in registered_entity_types {
                let _ = table_change_tx.send(RegistryTableChange {
                    tenant: tenant.clone(),
                    entity_type,
                    version: 1,
                });
            }
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
}
