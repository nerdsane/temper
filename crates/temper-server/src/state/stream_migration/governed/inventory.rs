//! Bounded inventory scanning and descriptor publication.

use super::super::StreamDescriptorBackfillContext;
use super::*;

impl ServerState {
    pub(super) async fn scan_inventory_page(
        &self,
        tenant: &TenantId,
        job: &mut DurableJobV1,
    ) -> Result<(), String> {
        let (journal, _) = self
            .event_journal()
            .ok_or_else(|| "backend unavailable: event journal is unavailable".to_string())?;
        let page_limit = usize::try_from(job.budgets.max_subjects)
            .map_err(|_| "subject budget exceeds platform size")?;
        let mut consumed_bytes = 0_u64;
        let mut remaining = page_limit;
        while remaining > 0 && job.capability_index < job.capabilities.len() {
            let capability = job.capabilities[job.capability_index].clone();
            let entity_type = local_type(&capability.subject_type);
            let rows = match (&job.target, job.source_bundle_digest.as_deref()) {
                (
                    StreamDescriptorMigrationTargetV1::TaskBundle { scope, .. },
                    Some(source_digest),
                ) => {
                    let scope = SchemaScope {
                        kind: SchemaScopeKind::Task,
                        id: scope.id.clone(),
                    };
                    journal
                        .list_scoped_entity_ids_page(
                            tenant.as_str(),
                            entity_type,
                            &scope,
                            source_digest,
                            job.after_entity_id.as_deref(),
                            remaining.saturating_add(1),
                        )
                        .await
                        .map_err(persistence_error)?
                }
                (StreamDescriptorMigrationTargetV1::TaskBundle { .. }, None) => Vec::new(),
                (StreamDescriptorMigrationTargetV1::InstalledApplication { .. }, _) => journal
                    .list_unscoped_entity_ids_page(
                        tenant.as_str(),
                        entity_type,
                        job.after_entity_id.as_deref(),
                        remaining.saturating_add(1),
                    )
                    .await
                    .map_err(persistence_error)?,
            };
            let has_more = rows.len() > remaining;
            for entity_id in rows.into_iter().take(remaining) {
                let outcome = self
                    .migrate_inventory_subject(
                        tenant,
                        job,
                        &capability,
                        &entity_id,
                        &mut consumed_bytes,
                    )
                    .await;
                job.scanned_subjects = job
                    .scanned_subjects
                    .checked_add(1)
                    .ok_or_else(|| "migration subject count overflowed".to_string())?;
                apply_outcome(job, entity_type, &entity_id, outcome)?;
                job.after_entity_id = Some(entity_id);
                remaining -= 1;
            }
            if has_more || remaining == 0 {
                break;
            }
            job.capability_index += 1;
            job.after_entity_id = None;
        }
        job.scan_complete = job.capability_index >= job.capabilities.len();
        Ok(())
    }

    pub(super) async fn stream_publication_generation(
        &self,
        tenant: &TenantId,
        target: &StreamDescriptorMigrationTargetV1,
        source_bundle_digest: Option<&str>,
        capabilities: &[VerifiedStreamCapabilityV1],
    ) -> Result<PublicationGenerationV1, String> {
        let (journal, _) = self
            .event_journal()
            .ok_or_else(|| "backend unavailable: event journal is unavailable".to_string())?;
        if let (
            StreamDescriptorMigrationTargetV1::TaskBundle { scope, .. },
            Some(source_bundle_digest),
        ) = (target, source_bundle_digest)
        {
            let scope = SchemaScope {
                kind: SchemaScopeKind::Task,
                id: scope.id.clone(),
            };
            let generation = journal
                .scoped_bundle_write_version(tenant.as_str(), &scope, source_bundle_digest)
                .await
                .map_err(persistence_error)?;
            return Ok(PublicationGenerationV1 {
                token: format!("scoped:{generation}"),
                scoped_write_version: Some(generation),
                unscoped_write_versions: BTreeMap::new(),
            });
        }
        if matches!(target, StreamDescriptorMigrationTargetV1::TaskBundle { .. }) {
            return Ok(PublicationGenerationV1 {
                token: "scoped:0".into(),
                scoped_write_version: Some(0),
                unscoped_write_versions: BTreeMap::new(),
            });
        }
        let mut hasher = Sha256::new();
        let mut unscoped_write_versions = BTreeMap::new();
        for capability in capabilities {
            let entity_type = local_type(&capability.subject_type);
            let version = journal
                .unscoped_entity_type_write_version(tenant.as_str(), entity_type)
                .await
                .map_err(persistence_error)?;
            hasher.update(entity_type.as_bytes());
            hasher.update([0]);
            hasher.update(version.to_be_bytes());
            unscoped_write_versions.insert(entity_type.to_string(), version);
        }
        Ok(PublicationGenerationV1 {
            token: format!("global-sha256:{:x}", hasher.finalize()),
            scoped_write_version: None,
            unscoped_write_versions,
        })
    }

    pub(super) async fn retry_unresolved_page(
        &self,
        tenant: &TenantId,
        job: &mut DurableJobV1,
    ) -> Result<(), String> {
        let page_limit = usize::try_from(job.budgets.max_subjects)
            .map_err(|_| "subject budget exceeds platform size")?;
        let ordered_subjects = ordered_unresolved_subjects(&job.unresolved)?;
        let mut subjects = ordered_subjects
            .iter()
            .filter(|key| job.retry_after.as_ref().is_none_or(|after| *key > after))
            .take(page_limit)
            .cloned()
            .collect::<Vec<_>>();
        if subjects.len() < page_limit {
            subjects.extend(
                ordered_subjects
                    .iter()
                    .filter(|key| job.retry_after.as_ref().is_some_and(|after| *key <= after))
                    .take(page_limit - subjects.len())
                    .cloned(),
            );
        }
        let mut consumed_bytes = 0_u64;
        for (entity_type, entity_id) in subjects {
            let Some(capability) = job
                .capabilities
                .iter()
                .find(|capability| local_type(&capability.subject_type) == entity_type)
                .cloned()
            else {
                continue;
            };
            let outcome = self
                .migrate_inventory_subject(
                    tenant,
                    job,
                    &capability,
                    &entity_id,
                    &mut consumed_bytes,
                )
                .await;
            apply_outcome(job, &entity_type, &entity_id, outcome)?;
            job.retry_after = Some((entity_type, entity_id));
        }
        Ok(())
    }

    pub(super) async fn migrate_inventory_subject(
        &self,
        tenant: &TenantId,
        job: &DurableJobV1,
        capability: &VerifiedStreamCapabilityV1,
        entity_id: &str,
        consumed_bytes: &mut u64,
    ) -> Result<StreamDescriptorBackfillOutcomeV1, String> {
        let (journal, _) = self
            .event_journal()
            .ok_or_else(|| "backend unavailable: event journal is unavailable".to_string())?;
        let entity_type = local_type(&capability.subject_type);
        let source_pin = source_pin(job);
        let journal_entity_id = source_pin.as_ref().map_or_else(
            || entity_id.to_string(),
            |pin| scoped_journal_entity_id(entity_id, pin),
        );
        let persistence_id = format!("{tenant}:{entity_type}:{journal_entity_id}");
        let event_limit = usize::try_from(job.budgets.max_events_per_subject)
            .map_err(|_| "event budget exceeds platform size")?;
        let events = journal
            .read_latest_events(&persistence_id, event_limit.saturating_add(1))
            .await
            .map_err(persistence_error)?;
        if events.len() > event_limit {
            return Err("event_budget_exceeded".into());
        }
        if events.is_empty() {
            return Err("missing_journal".into());
        }
        let provenance = capability
            .migration_provenance
            .as_ref()
            .ok_or_else(|| "missing_verified_provenance".to_string())?;
        let publication_events = events
            .iter()
            .filter(|event| event.event_type == provenance.publication_action)
            .collect::<Vec<_>>();
        let content_event = publication_events
            .last()
            .copied()
            .ok_or_else(|| "missing_publication".to_string())?;
        if capability.mutability == StreamCapabilityMutabilityV1::Immutable
            && publication_events.len() != 1
        {
            return Err("ambiguous_immutable_publication".into());
        }
        let facts = historical_stream_facts(
            content_event,
            provenance,
            entity_type,
            capability.authorization_parent_type.as_deref(),
        )
        .map_err(|_| "invalid_publication".to_string())?;
        let next_bytes = consumed_bytes
            .checked_add(facts.byte_length)
            .ok_or_else(|| "blob_budget_exceeded".to_string())?;
        if next_bytes > job.budgets.max_blob_bytes {
            return Err("blob_budget_exceeded".into());
        }
        *consumed_bytes = next_bytes;
        let candidate = candidate(
            entity_type,
            entity_id,
            content_event.sequence_nr,
            events.last().map_or(0, |event| event.sequence_nr),
            capability,
            provenance,
            facts,
        )?;
        self.backfill_stream_descriptor_v1_inner(
            tenant,
            &candidate,
            StreamDescriptorBackfillContext {
                journal_entity_id: &journal_entity_id,
                eviction_pin: source_pin.as_ref(),
                provenance,
                authorization_parent_type: capability.authorization_parent_type.as_deref(),
                verified_capability: Some(capability),
            },
        )
        .await
    }

    pub(super) async fn load_governed_job(
        &self,
        tenant: &TenantId,
        job_id: &str,
    ) -> Result<Option<DurableJobV1>, String> {
        let (journal, _) = self
            .event_journal()
            .ok_or_else(|| "backend unavailable: event journal is unavailable".to_string())?;
        let persistence_id = job_persistence_id(tenant, job_id);
        let events = journal
            .read_latest_events(&persistence_id, JOB_EVENT_BUDGET.saturating_add(1))
            .await
            .map_err(persistence_error)?;
        if events.len() > JOB_EVENT_BUDGET {
            return Err("stream descriptor migration job exceeded its event budget".into());
        }
        let Some(event) = events.last() else {
            return Ok(None);
        };
        if event.event_type != JOB_EVENT {
            return Err("stream descriptor migration journal is invalid".into());
        }
        serde_json::from_value(event.payload.clone())
            .map(Some)
            .map_err(|error| format!("stream descriptor migration job is invalid: {error}"))
    }

    pub(super) async fn persist_governed_job(
        &self,
        tenant: &TenantId,
        job: &mut DurableJobV1,
    ) -> Result<(), String> {
        let (journal, _) = self
            .event_journal()
            .ok_or_else(|| "backend unavailable: event journal is unavailable".to_string())?;
        let persistence_id = job_persistence_id(tenant, &job.job_id);
        let expected = job.committed_sequence;
        job.committed_sequence = expected
            .checked_add(1)
            .ok_or_else(|| "migration sequence overflowed".to_string())?;
        let payload = serde_json::to_value(&*job).map_err(|error| error.to_string())?;
        let payload_bytes = serde_json::to_vec(&payload).map_err(|error| error.to_string())?;
        if payload_bytes.len() > JOB_PAYLOAD_BYTE_BUDGET {
            job.committed_sequence = expected;
            return Err("stream descriptor migration job exceeded its payload byte budget".into());
        }
        let event = PersistenceEnvelope {
            sequence_nr: job.committed_sequence,
            event_type: JOB_EVENT.into(),
            payload,
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp: sim_now(),
                actor_id: persistence_id.clone(),
                kernel: None,
            },
        };
        if let Err(error) = journal.append(&persistence_id, expected, &[event]).await {
            job.committed_sequence = expected;
            return Err(persistence_error(error));
        }
        Ok(())
    }
}

fn ordered_unresolved_subjects(
    unresolved: &BTreeMap<String, String>,
) -> Result<Vec<(String, String)>, String> {
    let mut subjects = unresolved
        .keys()
        .map(|key| parse_unresolved_subject_key(key))
        .collect::<Result<Vec<_>, _>>()?;
    subjects.sort();
    Ok(subjects)
}

fn candidate(
    entity_type: &str,
    entity_id: &str,
    content_event_sequence: u64,
    expected_current_sequence: u64,
    capability: &VerifiedStreamCapabilityV1,
    provenance: &temper_spec::csdl::VerifiedStreamMigrationProvenanceV1,
    facts: HistoricalStreamFacts,
) -> Result<StreamDescriptorBackfillCandidateV1, String> {
    if !is_canonical_sha256(&facts.content_hash) {
        return Err("invalid_content_hash".into());
    }
    Ok(StreamDescriptorBackfillCandidateV1 {
        entity_type: entity_type.into(),
        entity_id: entity_id.into(),
        storage_object_id: format!("{}{}", provenance.storage_key_prefix, facts.content_hash),
        content_hash: facts.content_hash,
        byte_length: facts.byte_length,
        content_type: facts.content_type,
        content_event_sequence,
        expected_current_sequence,
        mutability: match capability.mutability {
            StreamCapabilityMutabilityV1::Mutable => StreamMutability::Mutable,
            StreamCapabilityMutabilityV1::Immutable => StreamMutability::Immutable,
        },
    })
}

fn apply_outcome(
    job: &mut DurableJobV1,
    entity_type: &str,
    entity_id: &str,
    outcome: Result<StreamDescriptorBackfillOutcomeV1, String>,
) -> Result<(), String> {
    let key = (entity_type.to_string(), entity_id.to_string());
    let unresolved_key = unresolved_subject_key(entity_type, entity_id)?;
    let subject_digest = format!(
        "sha256:{:x}",
        Sha256::digest([entity_type.as_bytes(), b"\0", entity_id.as_bytes()].concat())
    );
    match outcome {
        Ok(StreamDescriptorBackfillOutcomeV1::Appended { .. }) => {
            record_resolved(job, &key)?;
            job.unresolved.remove(&unresolved_key);
            job.latest_page_outcomes
                .push(StreamDescriptorMigrationPageOutcomeV1 {
                    subject_digest,
                    classification: "migrated".into(),
                });
        }
        Ok(StreamDescriptorBackfillOutcomeV1::AlreadyPresent { .. }) => {
            record_resolved(job, &key)?;
            job.unresolved.remove(&unresolved_key);
            job.latest_page_outcomes
                .push(StreamDescriptorMigrationPageOutcomeV1 {
                    subject_digest,
                    classification: "already_present".into(),
                });
        }
        Err(reason)
            if reason.starts_with("backend unavailable:") || reason.starts_with("stale fence:") =>
        {
            return Err(reason);
        }
        Ok(StreamDescriptorBackfillOutcomeV1::Unresolved { reason }) | Err(reason) => {
            if job.resolved.remove(&key) {
                job.migrated_subjects = job
                    .migrated_subjects
                    .checked_sub(1)
                    .ok_or_else(|| "migration migrated-subject count underflowed".to_string())?;
            }
            let classification = bounded_classification(&reason);
            job.unresolved
                .insert(unresolved_key, classification.clone());
            job.latest_page_outcomes
                .push(StreamDescriptorMigrationPageOutcomeV1 {
                    subject_digest,
                    classification,
                });
        }
    }
    Ok(())
}

fn record_resolved(job: &mut DurableJobV1, key: &(String, String)) -> Result<(), String> {
    if job.resolved.insert(key.clone()) {
        job.migrated_subjects = job
            .migrated_subjects
            .checked_add(1)
            .ok_or_else(|| "migration migrated-subject count overflowed".to_string())?;
    }
    Ok(())
}

fn bounded_classification(reason: &str) -> String {
    let value = reason.split(':').next().unwrap_or("unresolved");
    value.chars().take(96).collect()
}

fn source_pin(job: &DurableJobV1) -> Option<SchemaExecutionPin> {
    match (&job.target, &job.source_bundle_digest) {
        (StreamDescriptorMigrationTargetV1::TaskBundle { scope, .. }, Some(bundle_digest)) => {
            Some(SchemaExecutionPin {
                scope: SchemaScope {
                    kind: SchemaScopeKind::Task,
                    id: scope.id.clone(),
                },
                bundle_digest: bundle_digest.clone(),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unresolved_retry_order_uses_decoded_subjects() {
        let mut unresolved = BTreeMap::new();
        for entity_id in ["A", "\"quoted"] {
            unresolved.insert(
                unresolved_subject_key("File", entity_id).unwrap(),
                "missing_blob".into(),
            );
        }
        assert_eq!(
            ordered_unresolved_subjects(&unresolved).unwrap(),
            vec![
                ("File".into(), "\"quoted".into()),
                ("File".into(), "A".into())
            ]
        );
    }
}
