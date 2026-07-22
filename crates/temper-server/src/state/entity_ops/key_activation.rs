//! Declared-key activation and projection backfill entry points.

use super::*;

impl ServerState {
    /// Populate the durable query-plane projections for collection reads.
    ///
    /// Two-phase approach:
    /// 1. **Snapshot pass** — cheap: deserialises snapshots for entities that have them.
    /// 2. **Persistence replay pass** — reconstructs state directly from the event log.
    ///
    /// Runs once as a background task after startup.  New entities created after
    /// boot are indexed via `run_post_dispatch_effects` step 8.
    #[instrument(skip_all, fields(otel.name = "entity.populate_field_index", tenant = %tenant))]
    pub async fn populate_field_index_from_snapshots(&self, tenant: &TenantId) {
        projection_backfill::populate_field_index_from_snapshots(self, tenant).await;
    }

    /// Backfill `entity_key_index` for declared-key entity types (ADR-0153), so a
    /// keyed read can authoritatively prove absence for pre-existing entities.
    ///
    /// Independent of [`Self::populate_field_index_from_snapshots`]: the declared
    /// key is `K` (1–3) tiny rows per entity, far cheaper than the broad `S`-wide
    /// field-index re-scan, so it is gated and scheduled on its own rather than
    /// riding the expensive projection backfill. Runs once as a background task;
    /// entities written after boot are keyed inline at write time.
    #[instrument(skip_all, fields(otel.name = "entity.populate_key_index", tenant = %tenant))]
    pub async fn populate_key_index_from_snapshots(&self, tenant: &TenantId) {
        projection_backfill::populate_key_index_from_snapshots(self, tenant).await;
    }

    /// Fence declared-key contracts before publishing replacement specs.
    ///
    /// This invalidates an older coverage proof synchronously with activation
    /// preparation. An empty contract also purges the type's old ownership rows
    /// atomically, so a rapid A -> none -> A cycle cannot resurrect either the
    /// original watermark or claims that should have been released.
    pub async fn prepare_key_index_contracts_for_spec_activation(
        &self,
        publication_guard: &SpecPublicationGuard,
        tenant: &TenantId,
        ioa_sources: &[(&str, &str)],
    ) -> Result<KeyContractActivationCutover, String> {
        self.prepare_key_index_contracts_for_spec_activation_with_removals(
            publication_guard,
            tenant,
            ioa_sources,
            &[],
        )
        .await
    }

    /// Fence a complete replacement, including entity types omitted by the new
    /// registry. Removed types activate an empty contract and purge their
    /// ownership rows in the same backend transaction as changed types.
    pub async fn prepare_key_index_contracts_for_spec_activation_with_removals(
        &self,
        publication_guard: &SpecPublicationGuard,
        tenant: &TenantId,
        ioa_sources: &[(&str, &str)],
        removed_entity_types: &[String],
    ) -> Result<KeyContractActivationCutover, String> {
        publication_guard.validates(tenant)?;
        let Some(storage) = self.storage_stack.as_ref() else {
            return Ok(KeyContractActivationCutover::empty());
        };
        let store = &storage.events;
        if !store.supports_authoritative_key_index() {
            return Ok(KeyContractActivationCutover::empty());
        }

        let mut contracts = BTreeMap::new();
        let mut candidate_tables = Vec::new();
        for (entity_type, ioa_source) in ioa_sources {
            if contracts.contains_key(*entity_type) {
                return Err(format!(
                    "duplicate entity type {entity_type} in key contract activation"
                ));
            }
            let table = temper_jit::table::TransitionTable::try_from_ioa_source(ioa_source)
                .map_err(|error| {
                    format!(
                        "failed to prepare key contract for {entity_type} before spec activation: {error}"
                    )
                })?;
            contracts.insert(
                (*entity_type).to_string(),
                (
                    crate::key_index::declared_key_set_signature(&table.keys),
                    format!("{:x}", Sha256::digest(ioa_source.as_bytes())),
                    table.keys.is_empty(),
                ),
            );
            candidate_tables.push(((*entity_type).to_string(), table));
        }
        for entity_type in removed_entity_types {
            if contracts.contains_key(entity_type) {
                return Err(format!(
                    "entity type {entity_type} cannot be both published and removed"
                ));
            }
            let removed_fingerprint = format!(
                "{:x}",
                Sha256::digest(format!("temper.removed-spec.v1:{entity_type}").as_bytes())
            );
            contracts.insert(
                entity_type.clone(),
                (
                    crate::key_index::declared_key_set_signature(&[]),
                    removed_fingerprint,
                    true,
                ),
            );
        }

        let activations = contracts
            .into_iter()
            .map(
                |(entity_type, (key_set, spec_fingerprint, purge_existing_rows))| {
                    temper_runtime::persistence::KeyContractActivation {
                        entity_type,
                        key_set,
                        spec_fingerprint,
                        purge_existing_rows,
                    }
                },
            )
            .collect::<Vec<_>>();
        let affected_entity_types = activations
            .iter()
            .map(|activation| activation.entity_type.clone())
            .collect::<std::collections::BTreeSet<_>>();
        {
            let mut activating = self
                .activating_key_contracts
                .write()
                .expect("activating key contracts lock poisoned");
            activating.extend(
                affected_entity_types
                    .iter()
                    .map(|entity_type| (tenant.as_str().to_string(), entity_type.clone())),
            );
        }
        let activation_epochs = match store
            .activate_key_index_contracts(tenant.as_str(), &activations)
            .await
        {
            Ok(epochs) => epochs,
            Err(error) => {
                return Err(format!(
                    "failed to atomically activate key contracts for {tenant}: {error}"
                ));
            }
        };
        for (entity_type, table) in &mut candidate_tables {
            if let Some(epoch) = activation_epochs.get(entity_type) {
                table.key_contract_activation_epoch = *epoch;
            }
        }
        for entity_type in activation_epochs.keys() {
            self.key_index_backfilled
                .write()
                .expect("key index backfilled lock poisoned")
                .remove(&format!("{tenant}:{entity_type}"));
        }
        let prepared_coverage = projection_backfill::prepare_key_index_coverage_for_activation(
            self,
            tenant,
            &candidate_tables,
        )
        .await?;
        Ok(KeyContractActivationCutover {
            activation_epochs,
            candidate_tables,
            prepared_coverage,
            affected_entity_types,
        })
    }

    /// Complete a prepared cutover after the registry has published the supplied
    /// epochs. Spawn remains gated until every candidate coverage CAS succeeds.
    pub async fn finish_key_index_contract_activation(
        &self,
        publication_guard: &mut SpecPublicationGuard,
        tenant: &TenantId,
        cutover: &mut KeyContractActivationCutover,
    ) -> Result<(), String> {
        publication_guard.validates(tenant)?;
        self.evict_key_contract_actors(tenant, &cutover.affected_entity_types);
        const REPAIR_ATTEMPTS: usize = 3;
        let mut last_error = None;
        for attempt in 1..=REPAIR_ATTEMPTS {
            match projection_backfill::publish_prepared_key_index_coverage(
                self,
                tenant,
                &cutover.prepared_coverage,
            )
            .await
            {
                Ok(()) => {
                    let mut activating = self
                        .activating_key_contracts
                        .write()
                        .expect("activating key contracts lock poisoned");
                    for entity_type in &cutover.affected_entity_types {
                        activating.remove(&(tenant.as_str().to_string(), entity_type.clone()));
                    }
                    return Ok(());
                }
                Err(error) if attempt < REPAIR_ATTEMPTS => {
                    last_error = Some(error);
                    cutover.prepared_coverage =
                        projection_backfill::prepare_key_index_coverage_for_activation(
                            self,
                            tenant,
                            &cutover.candidate_tables,
                        )
                        .await?;
                }
                Err(error) => last_error = Some(error),
            }
        }
        Err(format!(
            "key contract activation for {tenant} remained not-ready after {REPAIR_ATTEMPTS} source-fenced repairs: {}",
            last_error.unwrap_or_else(|| "unknown readiness failure".to_string())
        ))
    }

    /// Publish every non-key runtime dependency, evict any actor that crossed
    /// the arm/slow-spawn boundary, and reopen the tenant as the final cutover
    /// step. Callers must not invoke this until registry, reactions, policies,
    /// and required integration modules match the durable generation.
    pub fn complete_spec_publication(
        &self,
        publication_guard: &mut SpecPublicationGuard,
        tenant: &TenantId,
    ) -> Result<(), String> {
        self.complete_spec_publication_inner(publication_guard, tenant, false)
    }

    /// Reopen a tenant after an exact retry of the complete runtime-generation
    /// intent that originally left it fail-closed. Partial repair helpers must
    /// use [`Self::complete_spec_publication`] so they cannot discharge debt for
    /// missing policy, WASM, or unrelated durable specs.
    pub fn complete_spec_publication_retry(
        &self,
        publication_guard: &mut SpecPublicationGuard,
        tenant: &TenantId,
    ) -> Result<(), String> {
        self.complete_spec_publication_inner(publication_guard, tenant, true)
    }

    pub(super) fn complete_spec_publication_inner(
        &self,
        publication_guard: &mut SpecPublicationGuard,
        tenant: &TenantId,
        allow_inherited_debt: bool,
    ) -> Result<(), String> {
        publication_guard.validates(tenant)?;
        self.evict_tenant_actors(tenant);
        publication_guard.release(allow_inherited_debt)
    }

    pub(super) fn evict_key_contract_actors(
        &self,
        tenant: &TenantId,
        affected_entity_types: &std::collections::BTreeSet<String>,
    ) {
        self.evict_tenant_actors_for_types(tenant, Some(affected_entity_types));
    }

    pub(super) fn evict_tenant_actors(&self, tenant: &TenantId) {
        self.evict_tenant_actors_for_types(tenant, None);
    }

    pub(super) fn evict_tenant_actors_for_types(
        &self,
        tenant: &TenantId,
        affected_entity_types: Option<&std::collections::BTreeSet<String>>,
    ) {
        let removed = {
            let mut actors = self
                .actor_registry
                .write()
                .expect("actor registry lock poisoned");
            let keys = actors
                .keys()
                .filter(|actor_key| {
                    temper_runtime::tenant::parse_persistence_id_parts(actor_key)
                        .ok()
                        .is_some_and(|(actor_tenant, entity_type, _)| {
                            actor_tenant == tenant.as_str()
                                && affected_entity_types
                                    .is_none_or(|types| types.contains(entity_type))
                        })
                })
                .cloned()
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| actors.remove(&key).map(|actor| (key, actor)))
                .collect::<Vec<_>>()
        };
        for (_, actor) in &removed {
            let _ = actor.stop();
        }
        if let Ok(mut last_accessed) = self.last_accessed.write() {
            for (actor_key, _) in removed {
                last_accessed.remove(&actor_key);
            }
        }
    }

    /// Activate every currently registered key contract for a tenant before
    /// startup begins serving traffic. Activated types omitted by the durable
    /// registry are retired to an empty contract, closing both persist-first
    /// crash boundaries without reviving older ownership authority.
    pub async fn activate_registered_key_contracts(&self, tenant: &TenantId) -> Result<(), String> {
        let mut publication_guard = self.begin_spec_publication(tenant).await?;
        let sources = {
            let registry = self.registry.read().expect("spec registry lock poisoned");
            registry
                .entity_types(tenant)
                .into_iter()
                .filter_map(|entity_type| {
                    registry
                        .get_spec(tenant, entity_type)
                        .map(|spec| (entity_type.to_string(), spec.ioa_source.clone()))
                })
                .collect::<Vec<_>>()
        };
        let intent_components = sources
            .iter()
            .map(|(entity_type, source)| (entity_type.as_str(), source.as_bytes()))
            .collect::<Vec<_>>();
        let intent = Self::spec_publication_intent("startup-full-registry", intent_components);
        self.arm_spec_publication(&mut publication_guard, tenant, &intent)?;
        let registered_types = sources
            .iter()
            .map(|(entity_type, _)| entity_type.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let removed_entity_types = if let Some(storage) = self.storage_stack.as_ref() {
            storage
                .events
                .key_index_activated_contracts()
                .await
                .map_err(|error| {
                    format!("failed to enumerate activated key contracts at startup: {error}")
                })?
                .into_iter()
                .filter(|(activated_tenant, entity_type)| {
                    activated_tenant == tenant.as_str()
                        && !registered_types.contains(entity_type.as_str())
                })
                .map(|(_, entity_type)| entity_type)
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let source_refs = sources
            .iter()
            .map(|(entity_type, source)| (entity_type.as_str(), source.as_str()))
            .collect::<Vec<_>>();
        let mut cutover = self
            .prepare_key_index_contracts_for_spec_activation_with_removals(
                &publication_guard,
                tenant,
                &source_refs,
                &removed_entity_types,
            )
            .await?;
        let live_tables = {
            let registry = self.registry.read().expect("spec registry lock poisoned");
            cutover
                .activation_epochs
                .iter()
                .filter_map(|(entity_type, epoch)| {
                    registry
                        .get_table_live(tenant, entity_type)
                        .map(|table| (table, *epoch))
                })
                .collect::<Vec<_>>()
        };
        for (table, epoch) in live_tables {
            table
                .write()
                .expect("transition table lock poisoned")
                .key_contract_activation_epoch = epoch;
        }
        self.finish_key_index_contract_activation(&mut publication_guard, tenant, &mut cutover)
            .await?;
        self.complete_spec_publication_retry(&mut publication_guard, tenant)
    }
}
