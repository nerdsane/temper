//! Governed, target-bound stream descriptor inventory and migration.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use temper_runtime::persistence::schema_deployment::{
    SchemaExecutionPin, SchemaScope, SchemaScopeKind, StreamPublicationFence,
    scoped_journal_entity_id,
};
use temper_runtime::persistence::{
    EventMetadata, PersistenceEnvelope, PersistenceError, StreamMutability,
};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::{
    StreamCapabilityMutabilityV1, VerifiedStreamCapabilityV1, parse_csdl,
    stream_capability_set_digest_v1, verify_stream_capabilities_v1,
};
use temper_wasm_sdk::schema_deployment::{
    AdvanceStreamDescriptorMigrationRequestV1, GetStreamDescriptorMigrationRequestV1,
    ListUnresolvedStreamDescriptorsRequestV1, StartStreamDescriptorMigrationRequestV1,
    StreamDescriptorMigrationBudgetsV1, StreamDescriptorMigrationPageOutcomeV1,
    StreamDescriptorMigrationReceiptV1, StreamDescriptorMigrationTargetV1,
    UnresolvedStreamDescriptorPageV1, UnresolvedStreamDescriptorV1,
};

use super::{
    HistoricalStreamFacts, ServerState, StreamDescriptorBackfillCandidateV1,
    StreamDescriptorBackfillOutcomeV1, historical_stream_facts,
};

mod types;
use types::*;
mod reads;
mod target_stage;

fn persistence_error(error: PersistenceError) -> String {
    match error {
        PersistenceError::ConcurrencyViolation { .. } => format!("stale fence: {error}"),
        _ => format!("backend unavailable: {error}"),
    }
}

impl ServerState {
    /// Create or replay one durable target-bound migration job.
    pub async fn start_governed_stream_descriptor_migration_v1(
        &self,
        tenant: &TenantId,
        request: StartStreamDescriptorMigrationRequestV1,
    ) -> Result<StreamDescriptorMigrationReceiptV1, String> {
        validate_operation_id("request id", &request.request_id)?;
        validate_operation_id("idempotency key", &request.idempotency_key)?;
        validate_budgets(&request.budgets)?;
        if request.descriptor_contract_version != 1 {
            return Err("unsupported stream descriptor contract version".into());
        }
        let (capabilities, source_bundle_digest) = self
            .resolve_target_capabilities(tenant, &request.target)
            .await?;
        let active = capabilities
            .into_iter()
            .filter(|capability| capability.descriptor_contract_v1_active)
            .collect::<Vec<_>>();
        if active
            .iter()
            .any(|capability| capability.migration_provenance.is_none())
        {
            return Err("target stream capability has no verified migration provenance".into());
        }
        let capability_digest = stream_capability_set_digest_v1(&active)?;
        if capability_digest != request.expected_capability_digest {
            return Err("stream capability digest differs from the requested target".into());
        }
        let request_bytes = serde_json::to_vec(&(
            &request.target,
            &request.expected_capability_digest,
            request.descriptor_contract_version,
            &request.budgets,
        ))
        .map_err(|error| error.to_string())?;
        let request_digest = format!("sha256:{:x}", Sha256::digest(&request_bytes));
        let target_bytes =
            serde_json::to_vec(&request.target).map_err(|error| error.to_string())?;
        let job_id = format!(
            "sdm:{:x}",
            Sha256::digest([tenant.as_str().as_bytes(), &target_bytes].concat())
        );
        if let Some(existing) = self.load_governed_job(tenant, &job_id).await? {
            if existing.request_digest != request_digest
                || existing.idempotency_key != request.idempotency_key
            {
                return Err("stream descriptor migration idempotency conflict".into());
            }
            return existing
                .start_receipt
                .ok_or_else(|| "stream descriptor migration start receipt is absent".to_string());
        }
        let scan_generation = self
            .stream_publication_generation(
                tenant,
                &request.target,
                source_bundle_digest.as_deref(),
                &active,
            )
            .await?;
        let mut job = DurableJobV1 {
            contract_version: 1,
            request_digest,
            idempotency_key: request.idempotency_key,
            accepted_request_id: request.request_id.clone(),
            job_id,
            target: request.target,
            capability_digest,
            descriptor_contract_version: 1,
            budgets: request.budgets,
            capabilities: active,
            source_bundle_digest,
            capability_index: 0,
            after_entity_id: None,
            scan_complete: false,
            scanned_subjects: 0,
            migrated_subjects: 0,
            unresolved: BTreeMap::new(),
            resolved: BTreeSet::new(),
            latest_page_outcomes: Vec::new(),
            retry_after: None,
            scan_generation,
            completion_generation: None,
            completion_receipt_id: None,
            start_receipt: None,
            advance_operations: BTreeMap::new(),
            committed_sequence: 0,
        };
        if job.capabilities.is_empty() {
            job.scan_complete = true;
            job.completion_generation = Some(job.scan_generation.clone());
            job.completion_receipt_id = Some(completion_id(tenant, &job)?);
        }
        let receipt = job_receipt_at(&job, request.request_id, 1);
        job.start_receipt = Some(receipt.clone());
        self.persist_governed_job(tenant, &mut job).await?;
        tracing::info!(
            target_kind = target_kind(&job.target),
            descriptor_contract_version = 1_u16,
            unresolved_count = 0_u64,
            "stream descriptor migration started"
        );
        Ok(receipt)
    }

    /// Advance one bounded durable inventory page.
    pub async fn advance_governed_stream_descriptor_migration_v1(
        &self,
        tenant: &TenantId,
        request: AdvanceStreamDescriptorMigrationRequestV1,
    ) -> Result<StreamDescriptorMigrationReceiptV1, String> {
        validate_operation_id("request id", &request.request_id)?;
        validate_operation_id("idempotency key", &request.idempotency_key)?;
        validate_job_id(&request.job_id)?;
        let mut job = self
            .load_governed_job(tenant, &request.job_id)
            .await?
            .ok_or_else(|| "stream descriptor migration job was not found".to_string())?;
        let request_digest = format!("sha256:{:x}", Sha256::digest(request.job_id.as_bytes()));
        if let Some(replay) = job.advance_operations.get(&request.idempotency_key) {
            if replay.request_digest != request_digest {
                return Err("stream descriptor migration advance idempotency conflict".into());
            }
            return Ok(replay.receipt.clone());
        }
        if job.committed_sequence >= JOB_EVENT_BUDGET as u64 {
            return Err("stream descriptor migration operation budget is exhausted".into());
        }
        let (current_capabilities, source_bundle_digest) = self
            .resolve_target_capabilities(tenant, &job.target)
            .await?;
        let active = current_capabilities
            .into_iter()
            .filter(|capability| capability.descriptor_contract_v1_active)
            .collect::<Vec<_>>();
        if stream_capability_set_digest_v1(&active)? != job.capability_digest
            || source_bundle_digest != job.source_bundle_digest
        {
            return Err("stream descriptor migration target changed after job creation".into());
        }
        job.latest_page_outcomes.clear();
        if job.completion_receipt_id.is_some() {
            let generation = self
                .stream_publication_generation(
                    tenant,
                    &job.target,
                    job.source_bundle_digest.as_deref(),
                    &job.capabilities,
                )
                .await?;
            if job.completion_generation.as_ref() != Some(&generation) {
                job.scan_generation = generation;
                job.capability_index = 0;
                job.after_entity_id = None;
                job.scan_complete = false;
                job.completion_generation = None;
                job.completion_receipt_id = None;
            }
        }
        if job.scan_complete {
            self.retry_unresolved_page(tenant, &mut job).await?;
        } else {
            self.scan_inventory_page(tenant, &mut job).await?;
        }
        if job.scan_complete && job.unresolved.is_empty() {
            let generation = self
                .stream_publication_generation(
                    tenant,
                    &job.target,
                    job.source_bundle_digest.as_deref(),
                    &job.capabilities,
                )
                .await?;
            if generation == job.scan_generation {
                job.completion_generation = Some(generation);
                job.completion_receipt_id = Some(completion_id(tenant, &job)?);
            } else {
                job.scan_generation = generation;
                job.capability_index = 0;
                job.after_entity_id = None;
                job.scan_complete = false;
            }
        }
        let replay_key = request.idempotency_key;
        let projected_sequence = job
            .committed_sequence
            .checked_add(1)
            .ok_or_else(|| "migration sequence overflowed".to_string())?;
        let receipt = job_receipt_at(&job, request.request_id, projected_sequence);
        job.advance_operations.insert(
            replay_key,
            DurableAdvanceReplayV1 {
                request_digest,
                receipt: receipt.clone(),
            },
        );
        self.persist_governed_job(tenant, &mut job).await?;
        tracing::info!(
            target_kind = target_kind(&job.target),
            descriptor_contract_version = 1_u16,
            scanned_count = job.scanned_subjects,
            unresolved_count = job.unresolved.len(),
            migration_complete = job.completion_receipt_id.is_some(),
            "stream descriptor migration page committed"
        );
        Ok(receipt)
    }

    /// Verify exact terminal evidence immediately before activation.
    pub async fn require_stream_descriptor_completion_v1(
        &self,
        tenant: &TenantId,
        target: &StreamDescriptorMigrationTargetV1,
        receipt_id: Option<&str>,
    ) -> Result<Option<StreamPublicationFence>, String> {
        let (capabilities, _) = match self.resolve_target_capabilities(tenant, target).await {
            Ok(resolved) => resolved,
            Err(error) => {
                tracing::warn!(
                    target_kind = target_kind(target),
                    descriptor_contract_version = 1_u16,
                    gate_decision = "deny",
                    denial_reason = "target_resolution_failed",
                    "stream descriptor activation gate denied"
                );
                return Err(error);
            }
        };
        let active = capabilities
            .into_iter()
            .filter(|capability| capability.descriptor_contract_v1_active)
            .collect::<Vec<_>>();
        if active.is_empty() {
            return Ok(None);
        }
        let target_bytes = serde_json::to_vec(target).map_err(|error| error.to_string())?;
        let job_id = format!(
            "sdm:{:x}",
            Sha256::digest([tenant.as_str().as_bytes(), &target_bytes].concat())
        );
        let Some(job) = self.load_governed_job(tenant, &job_id).await? else {
            tracing::warn!(
                target_kind = target_kind(target),
                descriptor_contract_version = 1_u16,
                gate_decision = "deny",
                denial_reason = "completion_evidence_absent",
                "stream descriptor activation gate denied"
            );
            return Err("stream descriptor migration completion evidence is absent".into());
        };
        let receipt_matches = match target {
            StreamDescriptorMigrationTargetV1::TaskBundle { .. } => {
                job.completion_receipt_id.as_deref() == receipt_id
            }
            StreamDescriptorMigrationTargetV1::InstalledApplication { .. } => receipt_id
                .map_or(job.completion_receipt_id.is_some(), |receipt_id| {
                    job.completion_receipt_id.as_deref() == Some(receipt_id)
                }),
        };
        let current_generation = self
            .stream_publication_generation(
                tenant,
                target,
                job.source_bundle_digest.as_deref(),
                &active,
            )
            .await?;
        if job.capability_digest != stream_capability_set_digest_v1(&active)?
            || job.descriptor_contract_version != 1
            || !job.scan_complete
            || !job.unresolved.is_empty()
            || !receipt_matches
            || job.completion_generation.as_ref() != Some(&current_generation)
        {
            tracing::warn!(
                target_kind = target_kind(target),
                descriptor_contract_version = 1_u16,
                gate_decision = "deny",
                unresolved_count = job.unresolved.len(),
                "stream descriptor activation gate denied"
            );
            return Err(
                "matching zero-unresolved stream descriptor migration evidence is required".into(),
            );
        }
        tracing::info!(
            target_kind = target_kind(target),
            descriptor_contract_version = 1_u16,
            gate_decision = "allow",
            unresolved_count = 0_u64,
            "stream descriptor activation gate allowed"
        );
        match target {
            StreamDescriptorMigrationTargetV1::TaskBundle { .. } => {
                let Some(source_bundle_digest) = job.source_bundle_digest else {
                    return Ok(None);
                };
                let expected_write_version = job
                    .completion_generation
                    .as_ref()
                    .and_then(|generation| generation.scoped_write_version)
                    .ok_or_else(|| {
                        "stream descriptor migration generation is invalid".to_string()
                    })?;
                Ok(Some(StreamPublicationFence::TaskScoped {
                    source_bundle_digest,
                    expected_write_version,
                    bindings: active
                        .iter()
                        .map(|capability| {
                            capability
                                .migration_provenance
                                .as_ref()
                                .map(|provenance| {
                                    (
                                        local_type(&capability.subject_type).to_string(),
                                        provenance.publication_action.clone(),
                                    )
                                })
                                .ok_or_else(|| "task stream provenance is absent".to_string())
                        })
                        .collect::<Result<BTreeMap<_, _>, _>>()?,
                }))
            }
            StreamDescriptorMigrationTargetV1::InstalledApplication {
                application_id,
                semantic_digest,
            } => {
                let mut bindings = BTreeMap::new();
                let completed_generation = job.completion_generation.as_ref().ok_or_else(|| {
                    "stream descriptor migration generation is invalid".to_string()
                })?;
                for capability in &active {
                    let entity_type = local_type(&capability.subject_type).to_string();
                    let provenance = capability.migration_provenance.as_ref().ok_or_else(|| {
                        "installed application stream provenance is absent".to_string()
                    })?;
                    let expected_write_version = completed_generation
                        .unscoped_write_versions
                        .get(&entity_type)
                        .copied()
                        .ok_or_else(|| {
                            "stream descriptor migration generation is invalid".to_string()
                        })?;
                    bindings.insert(
                        entity_type,
                        temper_runtime::persistence::schema_deployment::UnscopedStreamPublicationBinding {
                            publication_action: provenance.publication_action.clone(),
                            capability_digest: temper_spec::csdl::stream_capability_set_digest_v1(
                                std::slice::from_ref(capability),
                            )?,
                            expected_write_version,
                        },
                    );
                }
                Ok(Some(StreamPublicationFence::InstalledApplication {
                    application_id: application_id.clone(),
                    semantic_digest: semantic_digest.clone(),
                    bindings,
                }))
            }
        }
    }

    async fn resolve_target_capabilities(
        &self,
        tenant: &TenantId,
        target: &StreamDescriptorMigrationTargetV1,
    ) -> Result<(Vec<VerifiedStreamCapabilityV1>, Option<String>), String> {
        match target {
            StreamDescriptorMigrationTargetV1::TaskBundle {
                scope,
                bundle_digest,
            } => {
                if scope.kind != "task" || scope.id.is_empty() {
                    return Err("stream descriptor migration requires a task scope".into());
                }
                let runtime_scope = SchemaScope {
                    kind: SchemaScopeKind::Task,
                    id: scope.id.clone(),
                };
                let record = self
                    .storage_stack
                    .as_ref()
                    .and_then(|stack| stack.schema_deployments.as_ref())
                    .ok_or_else(|| {
                        "backend unavailable: schema deployment store is unavailable".to_string()
                    })?
                    .get_schema_deployment(tenant.as_str(), &runtime_scope, bundle_digest)
                    .await
                    .map_err(|error| format!("backend unavailable: {error}"))?
                    .ok_or_else(|| "target schema bundle was not found".to_string())?;
                let document = parse_csdl(&record.bundle.canonical_csdl)
                    .map_err(|error| format!("target CSDL is invalid: {error}"))?;
                Ok((
                    verify_stream_capabilities_v1(&document).map_err(|error| error.to_string())?,
                    record.bundle.predecessor_digest,
                ))
            }
            StreamDescriptorMigrationTargetV1::InstalledApplication { .. } => {
                let StreamDescriptorMigrationTargetV1::InstalledApplication {
                    application_id,
                    semantic_digest,
                } = target
                else {
                    unreachable!()
                };
                let (journal, _) = self.event_journal().ok_or_else(|| {
                    "backend unavailable: event journal is unavailable".to_string()
                })?;
                let events = journal
                    .read_latest_events(
                        &staged_application_persistence_id(tenant, application_id),
                        1,
                    )
                    .await
                    .map_err(persistence_error)?;
                let staged: StagedInstalledApplicationV1 = serde_json::from_value(
                    events
                        .last()
                        .ok_or_else(|| {
                            "installed application stream target was not staged".to_string()
                        })?
                        .payload
                        .clone(),
                )
                .map_err(|error| format!("staged application stream target is invalid: {error}"))?;
                if staged.semantic_digest != *semantic_digest
                    || staged.application_id != *application_id
                {
                    return Err(
                        "installed application stream target differs from staged evidence".into(),
                    );
                }
                Ok((staged.capabilities, None))
            }
        }
    }
}

mod inventory;
