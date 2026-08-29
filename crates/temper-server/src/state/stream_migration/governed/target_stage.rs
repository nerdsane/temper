//! Durable installed-application target staging and publication-fence activation.

use super::*;

impl ServerState {
    /// Whether the exact installed application stream contract already has a durable fence.
    pub async fn installed_application_stream_contract_activated_v1(
        &self,
        tenant: &TenantId,
        application_id: &str,
        semantic_digest: &str,
        csdl_xml: &str,
    ) -> Result<bool, String> {
        let Some(fence) = self
            .installed_application_stream_fence_v1(tenant, application_id)
            .await?
        else {
            return Ok(false);
        };
        let StreamPublicationFence::InstalledApplication {
            application_id: fenced_application,
            semantic_digest: fenced_semantic,
            bindings,
        } = fence
        else {
            return Ok(false);
        };
        if fenced_application != application_id || fenced_semantic != semantic_digest {
            return Ok(false);
        }
        let document = parse_csdl(csdl_xml)
            .map_err(|error| format!("installed application CSDL is invalid: {error}"))?;
        let active = verify_stream_capabilities_v1(&document)
            .map_err(|error| format!("installed application stream contract is invalid: {error}"))?
            .into_iter()
            .filter(|capability| capability.descriptor_contract_v1_active)
            .collect::<Vec<_>>();
        if bindings.len() != active.len() {
            return Ok(false);
        }
        for capability in &active {
            let entity_type = local_type(&capability.subject_type);
            let Some(provenance) = capability.migration_provenance.as_ref() else {
                return Ok(false);
            };
            let expected_capability_digest =
                stream_capability_set_digest_v1(std::slice::from_ref(capability))?;
            if !bindings.get(entity_type).is_some_and(|binding| {
                binding.publication_action == provenance.publication_action
                    && binding.capability_digest == expected_capability_digest
            }) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Read an installed application's current fence with refreshed generations.
    pub async fn installed_application_stream_fence_v1(
        &self,
        tenant: &TenantId,
        application_id: &str,
    ) -> Result<Option<StreamPublicationFence>, String> {
        let journal = self
            .event_journal()
            .ok_or_else(|| "backend unavailable: event journal is unavailable".to_string())?
            .0;
        let Some(mut fence) = journal
            .get_unscoped_stream_publication_fence(tenant.as_str(), application_id)
            .await
            .map_err(persistence_error)?
        else {
            return Ok(None);
        };
        let StreamPublicationFence::InstalledApplication { bindings, .. } = &mut fence else {
            return Err("installed application publication fence is invalid".into());
        };
        for (entity_type, binding) in bindings {
            binding.expected_write_version = journal
                .unscoped_entity_type_write_version(tenant.as_str(), entity_type)
                .await
                .map_err(persistence_error)?;
        }
        Ok(Some(fence))
    }

    /// Atomically restore a prior installed-application fence during rollback.
    pub async fn restore_installed_application_stream_fence_v1(
        &self,
        tenant: &TenantId,
        expected_current_semantic_digest: &str,
        fence: &StreamPublicationFence,
    ) -> Result<(), String> {
        self.event_journal()
            .ok_or_else(|| "backend unavailable: event journal is unavailable".to_string())?
            .0
            .restore_unscoped_stream_publication_fence(
                tenant.as_str(),
                expected_current_semantic_digest,
                fence,
            )
            .await
            .map_err(persistence_error)
    }

    /// Atomically install a tenant-global publication fence after migration completion.
    pub async fn activate_installed_application_stream_fence_v1(
        &self,
        tenant: &TenantId,
        fence: &StreamPublicationFence,
    ) -> Result<(), String> {
        self.event_journal()
            .ok_or_else(|| "backend unavailable: event journal is unavailable".to_string())?
            .0
            .activate_unscoped_stream_publication_fence(tenant.as_str(), fence)
            .await
            .map_err(persistence_error)
    }

    /// Remove one exact installed-application publication fence.
    pub async fn deactivate_installed_application_stream_fence_v1(
        &self,
        tenant: &TenantId,
        application_id: &str,
        semantic_digest: Option<&str>,
    ) -> Result<(), String> {
        self.event_journal()
            .ok_or_else(|| "backend unavailable: event journal is unavailable".to_string())?
            .0
            .deactivate_unscoped_stream_publication_fence(
                tenant.as_str(),
                application_id,
                semantic_digest,
            )
            .await
            .map_err(persistence_error)
    }

    /// Verify and durably stage an installed application's exact stream target.
    pub async fn stage_installed_application_stream_migration_target_v1(
        &self,
        tenant: &TenantId,
        application_id: &str,
        semantic_digest: &str,
        csdl_xml: &str,
        ioa_sources: &[(String, String)],
    ) -> Result<Option<String>, String> {
        if application_id.is_empty() || semantic_digest.is_empty() {
            return Err("installed application migration target is invalid".into());
        }
        let document = parse_csdl(csdl_xml)
            .map_err(|error| format!("installed application CSDL is invalid: {error}"))?;
        let capabilities = verify_stream_capabilities_v1(&document).map_err(|error| {
            format!("installed application stream contract is invalid: {error}")
        })?;
        let automata = ioa_sources
            .iter()
            .map(|(entity_type, source)| {
                temper_spec::automaton::parse_automaton(source)
                    .map(|automaton| (entity_type.clone(), automaton))
                    .map_err(|error| {
                        format!("installed application IOA '{entity_type}' is invalid: {error}")
                    })
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        temper_spec::csdl::verify_stream_migration_automata_v1(&capabilities, &automata)?;
        let active = capabilities
            .into_iter()
            .filter(|capability| capability.descriptor_contract_v1_active)
            .collect::<Vec<_>>();
        if active.is_empty() {
            return Ok(None);
        }
        let staged = StagedInstalledApplicationV1 {
            application_id: application_id.into(),
            semantic_digest: semantic_digest.into(),
            capabilities: active,
        };
        let (journal, _) = self
            .event_journal()
            .ok_or_else(|| "backend unavailable: event journal is unavailable".to_string())?;
        let persistence_id = staged_application_persistence_id(tenant, application_id);
        let prior = journal
            .read_latest_events(&persistence_id, 1)
            .await
            .map_err(persistence_error)?;
        if let Some(event) = prior.last() {
            let existing: StagedInstalledApplicationV1 =
                serde_json::from_value(event.payload.clone()).map_err(|error| {
                    format!("staged application stream target is invalid: {error}")
                })?;
            if existing.application_id == staged.application_id
                && existing.semantic_digest == staged.semantic_digest
                && existing.capabilities == staged.capabilities
            {
                return stream_capability_set_digest_v1(&staged.capabilities).map(Some);
            }
        }
        let expected = prior.last().map_or(0, |event| event.sequence_nr);
        let sequence = expected
            .checked_add(1)
            .ok_or_else(|| "staged application target sequence overflowed".to_string())?;
        journal
            .append(
                &persistence_id,
                expected,
                &[PersistenceEnvelope {
                    sequence_nr: sequence,
                    event_type: STAGED_APPLICATION_EVENT.into(),
                    payload: serde_json::to_value(&staged).map_err(|error| error.to_string())?,
                    metadata: EventMetadata {
                        event_id: sim_uuid(),
                        causation_id: sim_uuid(),
                        correlation_id: sim_uuid(),
                        timestamp: sim_now(),
                        actor_id: persistence_id.clone(),
                        kernel: None,
                    },
                }],
            )
            .await
            .map_err(persistence_error)?;
        stream_capability_set_digest_v1(&staged.capabilities).map(Some)
    }
}
