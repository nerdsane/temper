//! Bounded, explicit migration of historical TemperFS stream descriptors.

use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use temper_runtime::persistence::{
    EventMetadata, KernelEventMetadata, PersistenceEnvelope, PersistenceError,
    StreamDescriptorInputV1, StreamDescriptorV1, StreamEntityRef, StreamMutability,
    StreamStorageRefV1,
};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::{
    StreamCapabilityMutabilityV1, VerifiedStreamCapabilityV1, VerifiedStreamMigrationProvenanceV1,
};

use crate::blob_store::BlobStreamRead;
use crate::entity_actor::EntityEvent;
use crate::storage::BoxedEventStore;

use super::ServerState;

mod governed;
mod report;
pub use report::StreamDescriptorMigrationPageReceiptV1;
use report::{DurableStreamDescriptorMigrationPageV1, MIGRATION_CURSOR_BYTE_BUDGET};

pub(crate) const STREAM_DESCRIPTOR_BACKFILLED_EVENT: &str = "_TemperStreamDescriptorBackfilledV1";
const BACKFILL_HISTORY_EVENT_BUDGET: usize = 1_024;

/// Bound authority and journal context for one descriptor repair.
pub(super) struct StreamDescriptorBackfillContext<'a> {
    journal_entity_id: &'a str,
    eviction_pin: Option<&'a temper_runtime::persistence::schema_deployment::SchemaExecutionPin>,
    provenance: &'a VerifiedStreamMigrationProvenanceV1,
    authorization_parent_type: Option<&'a str>,
    verified_capability: Option<&'a VerifiedStreamCapabilityV1>,
}
const BACKFILL_BATCH_ITEM_BUDGET: usize = 256;
const BACKFILL_BATCH_BYTE_BUDGET: usize = 1_048_576;

/// One exact historical stream selected by a separately audited inventory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamDescriptorBackfillCandidateV1 {
    /// Runtime entity type that owns the stream.
    pub entity_type: String,
    /// Entity identifier within the supplied tenant.
    pub entity_id: String,
    /// Inventoried platform digest, verified again against stored bytes.
    pub content_hash: String,
    /// Exact provider-opaque storage identity from the historical inventory.
    pub storage_object_id: String,
    /// Inventoried byte length, enforced before the verification read.
    pub byte_length: u64,
    /// Historical media type, when one was committed.
    pub content_type: Option<String>,
    /// Exact historical `StreamUpdated` publication sequence.
    pub content_event_sequence: u64,
    /// Journal fence captured by the inventory.
    pub expected_current_sequence: u64,
    /// Verified replacement semantics for this subject.
    pub mutability: StreamMutability,
}

/// Durable result for one bounded backfill candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamDescriptorBackfillOutcomeV1 {
    /// A new kernel-only descriptor event was appended.
    Appended {
        /// Committed journal sequence of the backfill event.
        descriptor_event_sequence: u64,
    },
    /// An idempotent rerun found the exact descriptor already committed.
    AlreadyPresent {
        /// Existing descriptor event sequence.
        descriptor_event_sequence: u64,
    },
    /// Verification failed without manufacturing descriptor authority.
    Unresolved {
        /// Bounded actionable failure classification.
        reason: String,
    },
}

impl ServerState {
    /// Verify and append at most 256 inventoried historical descriptors.
    pub async fn backfill_stream_descriptors_v1(
        &self,
        tenant: &TenantId,
        candidates: &[StreamDescriptorBackfillCandidateV1],
    ) -> Vec<StreamDescriptorBackfillOutcomeV1> {
        let encoded = match serde_json::to_vec(candidates) {
            Ok(encoded) => encoded,
            Err(error) => {
                return vec![StreamDescriptorBackfillOutcomeV1::Unresolved {
                    reason: format!("stream descriptor inventory encoding failed: {error}"),
                }];
            }
        };
        let cursor = format!("batch-sha256:{:x}", Sha256::digest(encoded));
        match self
            .backfill_stream_descriptor_inventory_page_v1(tenant, &cursor, false, candidates)
            .await
        {
            Ok(receipt) => receipt.outcomes,
            Err(reason) => vec![StreamDescriptorBackfillOutcomeV1::Unresolved { reason }],
        }
    }

    /// Process and durably report one bounded, resumable inventory page.
    pub async fn backfill_stream_descriptor_inventory_page_v1(
        &self,
        tenant: &TenantId,
        cursor: &str,
        final_page: bool,
        candidates: &[StreamDescriptorBackfillCandidateV1],
    ) -> Result<StreamDescriptorMigrationPageReceiptV1, String> {
        if cursor.is_empty()
            || cursor.trim() != cursor
            || cursor.len() > MIGRATION_CURSOR_BYTE_BUDGET
        {
            return Err("stream descriptor migration cursor is invalid or over budget".into());
        }
        if candidates.len() > BACKFILL_BATCH_ITEM_BUDGET {
            return Err(format!(
                "stream descriptor backfill batch exceeds {BACKFILL_BATCH_ITEM_BUDGET} items"
            ));
        }
        let encoded_candidates = serde_json::to_vec(candidates)
            .map_err(|error| format!("stream descriptor inventory encoding failed: {error}"))?;
        if encoded_candidates.len() > BACKFILL_BATCH_BYTE_BUDGET {
            return Err(format!(
                "stream descriptor backfill batch exceeds {BACKFILL_BATCH_BYTE_BUDGET} bytes"
            ));
        }
        let mut outcomes = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            outcomes.push(self.backfill_stream_descriptor_v1(tenant, candidate).await);
        }
        let migration_complete = final_page
            && outcomes.iter().all(|outcome| {
                !matches!(
                    outcome,
                    StreamDescriptorBackfillOutcomeV1::Unresolved { .. }
                )
            });
        self.persist_stream_descriptor_migration_page(
            tenant,
            DurableStreamDescriptorMigrationPageV1 {
                contract_version: 1,
                cursor: cursor.to_string(),
                final_page,
                candidates: candidates.to_vec(),
                outcomes,
                migration_complete,
            },
        )
        .await
    }

    async fn backfill_stream_descriptor_v1(
        &self,
        tenant: &TenantId,
        candidate: &StreamDescriptorBackfillCandidateV1,
    ) -> StreamDescriptorBackfillOutcomeV1 {
        let provenance = match legacy_temper_fs_provenance(&candidate.entity_type) {
            Ok(provenance) => provenance,
            Err(reason) => return StreamDescriptorBackfillOutcomeV1::Unresolved { reason },
        };
        match self
            .backfill_stream_descriptor_v1_inner(
                tenant,
                candidate,
                StreamDescriptorBackfillContext {
                    journal_entity_id: &candidate.entity_id,
                    eviction_pin: None,
                    provenance: &provenance,
                    authorization_parent_type: legacy_temper_fs_parent_type(&candidate.entity_type),
                    verified_capability: None,
                },
            )
            .await
        {
            Ok(outcome) => outcome,
            Err(reason) => StreamDescriptorBackfillOutcomeV1::Unresolved { reason },
        }
    }

    pub(super) async fn backfill_stream_descriptor_v1_inner(
        &self,
        tenant: &TenantId,
        candidate: &StreamDescriptorBackfillCandidateV1,
        context: StreamDescriptorBackfillContext<'_>,
    ) -> Result<StreamDescriptorBackfillOutcomeV1, String> {
        let (journal, _) = self
            .event_journal()
            .ok_or_else(|| "backend unavailable: event journal is unavailable".to_string())?;
        let persistence_id = format!(
            "{tenant}:{}:{}",
            candidate.entity_type, context.journal_entity_id
        );
        let events = journal
            .read_latest_events(
                &persistence_id,
                BACKFILL_HISTORY_EVENT_BUDGET.saturating_add(1),
            )
            .await
            .map_err(stream_migration_persistence_error)?;
        if events.len() > BACKFILL_HISTORY_EVENT_BUDGET {
            return Err("historical stream journal exceeds the backfill event budget".into());
        }
        let existing = events
            .iter()
            .filter_map(|event| event.metadata.kernel.as_ref())
            .map(KernelEventMetadata::stream_descriptor)
            .next_back()
            .cloned();
        if events.last().map_or(0, |event| event.sequence_nr) != candidate.expected_current_sequence
        {
            return Err("historical stream sequence changed during inventory".into());
        }
        let content_event = events
            .iter()
            .find(|event| event.sequence_nr == candidate.content_event_sequence)
            .ok_or_else(|| {
                "historical content event is outside the bounded inventory".to_string()
            })?;
        let expected_content_event = context.provenance.publication_action.as_str();
        if content_event.event_type != expected_content_event {
            return Err(format!(
                "historical content sequence is not a TemperFS {expected_content_event} event"
            ));
        }
        if events.iter().any(|event| {
            event.sequence_nr > candidate.content_event_sequence
                && event.event_type == expected_content_event
        }) {
            return Err("historical content sequence is not the latest publication".into());
        }
        let facts = historical_stream_facts(
            content_event,
            context.provenance,
            candidate.entity_type.as_str(),
            context.authorization_parent_type,
        )?;
        if facts.content_hash != candidate.content_hash
            || facts.byte_length != candidate.byte_length
            || candidate.content_type.as_deref() != facts.content_type.as_deref()
        {
            return Err("historical event content facts differ from inventory".into());
        }
        let object = match self
            .stream_blob_object(tenant, &candidate.storage_object_id, candidate.byte_length)
            .await
            .map_err(|error| format!("backend unavailable: {error}"))?
        {
            BlobStreamRead::Found(object) => object,
            BlobStreamRead::Missing => return Err("historical stream blob is missing".into()),
            BlobStreamRead::TooLarge { .. } => {
                return Err("historical stream blob exceeds inventoried length".into());
            }
        };
        if object.content_length() != candidate.byte_length {
            return Err("historical stream blob length differs from inventory".into());
        }
        let mut stream = object.into_stream();
        let mut actual_length = 0_u64;
        let mut hasher = Sha256::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| {
                format!("backend unavailable: historical stream read failed: {error}")
            })?;
            actual_length = actual_length
                .checked_add(u64::try_from(chunk.len()).map_err(|_| {
                    "historical stream chunk length exceeds platform size".to_string()
                })?)
                .ok_or_else(|| "historical stream length overflowed".to_string())?;
            if actual_length > candidate.byte_length {
                return Err("historical stream blob exceeds inventoried length".into());
            }
            hasher.update(&chunk);
        }
        if actual_length != candidate.byte_length {
            return Err("historical stream blob length differs from inventory".into());
        }
        let actual_hash = format!("sha256:{:x}", hasher.finalize());
        if actual_hash != candidate.content_hash {
            return Err("historical stream blob digest differs from inventory".into());
        }
        if let Some(existing) = existing {
            let exact = existing.subject().entity_type() == candidate.entity_type
                && existing.subject().entity_id() == candidate.entity_id
                && existing.authorization_parent() == facts.authorization_parent.as_ref()
                && existing.content_hash() == candidate.content_hash
                && existing.storage().object_id() == candidate.storage_object_id
                && existing.byte_length() == candidate.byte_length
                && existing.content_type() == candidate.content_type.as_deref()
                && existing.content_event_sequence() == candidate.content_event_sequence
                && existing.mutability() == candidate.mutability;
            if exact {
                return Ok(StreamDescriptorBackfillOutcomeV1::AlreadyPresent {
                    descriptor_event_sequence: existing.descriptor_event_sequence(),
                });
            }
            if existing.mutability() == StreamMutability::Immutable
                || candidate.mutability == StreamMutability::Immutable
            {
                return Err("a different immutable stream descriptor is already committed".into());
            }
            if candidate.content_event_sequence <= existing.content_event_sequence() {
                return Err("mutable stream descriptor candidate is not newer".into());
            }
        }
        let descriptor_sequence = candidate
            .expected_current_sequence
            .checked_add(1)
            .ok_or_else(|| "historical stream sequence overflowed".to_string())?;
        let descriptor = StreamDescriptorV1::new(StreamDescriptorInputV1 {
            subject: StreamEntityRef::new(&candidate.entity_type, &candidate.entity_id)
                .map_err(|error| error.to_string())?,
            authorization_parent: facts.authorization_parent,
            content_hash: candidate.content_hash.clone(),
            storage: StreamStorageRefV1::new(&candidate.storage_object_id)
                .map_err(|error| error.to_string())?,
            byte_length: candidate.byte_length,
            content_type: candidate.content_type.clone(),
            content_event_sequence: candidate.content_event_sequence,
            descriptor_event_sequence: descriptor_sequence,
            mutability: candidate.mutability,
        })
        .map_err(|error| error.to_string())?;
        if let Some(capability) = context.verified_capability {
            validate_verified_capability(&descriptor, capability)?;
        } else {
            self.validate_stream_descriptor_capability(tenant, None, &descriptor)?;
        }
        let latest_event: EntityEvent = serde_json::from_value(
            events
                .last()
                .ok_or_else(|| "historical stream journal is empty".to_string())?
                .payload
                .clone(),
        )
        .map_err(|error| format!("latest historical stream event is invalid: {error}"))?;
        let event = EntityEvent {
            action: STREAM_DESCRIPTOR_BACKFILLED_EVENT.into(),
            from_status: latest_event.to_status.clone(),
            to_status: latest_event.to_status,
            timestamp: sim_now(),
            params: serde_json::json!({
                "migration_provenance": BackfillProvenanceEvidenceV1 {
                    provenance: context.provenance.clone(),
                    authorization_parent_type: context
                        .authorization_parent_type
                        .map(str::to_string),
                },
            }),
            idempotency_key: None,
        };
        let mut payload = serde_json::to_value(event).map_err(|error| error.to_string())?;
        if let Some(pin) = context.eviction_pin
            && let Some(object) = payload.as_object_mut()
        {
            object.insert(
                crate::entity_actor::SCHEMA_PIN_FIELD.into(),
                serde_json::to_value(crate::entity_actor::schema_event_pin(
                    pin,
                    &candidate.entity_type,
                    STREAM_DESCRIPTOR_BACKFILLED_EVENT,
                ))
                .map_err(|error| error.to_string())?,
            );
        }
        let envelope = PersistenceEnvelope {
            sequence_nr: descriptor_sequence,
            event_type: STREAM_DESCRIPTOR_BACKFILLED_EVENT.into(),
            payload,
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp: sim_now(),
                actor_id: persistence_id.clone(),
                kernel: Some(KernelEventMetadata::V1 {
                    stream_descriptor: descriptor,
                }),
            },
        };
        journal
            .append(
                &persistence_id,
                candidate.expected_current_sequence,
                &[envelope],
            )
            .await
            .map_err(stream_migration_persistence_error)?;
        if let Some(pin) = context.eviction_pin {
            self.stop_and_remove_scoped_entity(
                tenant,
                &candidate.entity_type,
                &candidate.entity_id,
                pin,
            );
        } else {
            self.stop_and_remove_entity(tenant, &candidate.entity_type, &candidate.entity_id);
        }
        Ok(StreamDescriptorBackfillOutcomeV1::Appended {
            descriptor_event_sequence: descriptor_sequence,
        })
    }
}

fn stream_migration_persistence_error(error: PersistenceError) -> String {
    match error {
        PersistenceError::ConcurrencyViolation { .. } => format!("stale fence: {error}"),
        _ => format!("backend unavailable: {error}"),
    }
}

mod provenance;
pub(crate) use provenance::validate_backfill_replay_provenance;
use provenance::{
    BackfillProvenanceEvidenceV1, HistoricalStreamFacts, historical_stream_facts,
    legacy_temper_fs_parent_type, legacy_temper_fs_provenance, validate_verified_capability,
};
