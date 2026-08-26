//! Bounded, explicit migration of historical TemperFS stream descriptors.

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use temper_runtime::persistence::{
    EventMetadata, KernelEventMetadata, PersistenceEnvelope, StreamDescriptorV1, StreamEntityRef,
    StreamMutability, StreamStorageRefV1,
};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;

use crate::blob_store::BlobReadBounded;
use crate::entity_actor::EntityEvent;
use crate::storage::BoxedEventStore;

use super::ServerState;

mod report;
pub use report::StreamDescriptorMigrationPageReceiptV1;
use report::{DurableStreamDescriptorMigrationPageV1, MIGRATION_CURSOR_BYTE_BUDGET};

pub(crate) const STREAM_DESCRIPTOR_BACKFILLED_EVENT: &str = "_TemperStreamDescriptorBackfilledV1";
const BACKFILL_HISTORY_EVENT_BUDGET: usize = 1_024;
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
        match self
            .backfill_stream_descriptor_v1_inner(tenant, candidate)
            .await
        {
            Ok(outcome) => outcome,
            Err(reason) => StreamDescriptorBackfillOutcomeV1::Unresolved { reason },
        }
    }

    async fn backfill_stream_descriptor_v1_inner(
        &self,
        tenant: &TenantId,
        candidate: &StreamDescriptorBackfillCandidateV1,
    ) -> Result<StreamDescriptorBackfillOutcomeV1, String> {
        let (journal, _) = self
            .event_journal()
            .ok_or_else(|| "event journal is unavailable".to_string())?;
        let persistence_id = format!("{tenant}:{}:{}", candidate.entity_type, candidate.entity_id);
        let events = journal
            .read_latest_events(
                &persistence_id,
                BACKFILL_HISTORY_EVENT_BUDGET.saturating_add(1),
            )
            .await
            .map_err(|error| error.to_string())?;
        if events.len() > BACKFILL_HISTORY_EVENT_BUDGET {
            return Err("historical stream journal exceeds the backfill event budget".into());
        }
        if let Some(existing) = events
            .iter()
            .filter_map(|event| event.metadata.kernel.as_ref())
            .map(KernelEventMetadata::stream_descriptor)
            .next_back()
        {
            return if existing.subject().entity_type() == candidate.entity_type
                && existing.subject().entity_id() == candidate.entity_id
                && existing.content_hash() == candidate.content_hash
                && existing.storage().object_id() == candidate.storage_object_id
                && existing.byte_length() == candidate.byte_length
                && existing.content_event_sequence() == candidate.content_event_sequence
                && existing.mutability() == candidate.mutability
            {
                Ok(StreamDescriptorBackfillOutcomeV1::AlreadyPresent {
                    descriptor_event_sequence: existing.descriptor_event_sequence(),
                })
            } else {
                Err("a different stream descriptor is already committed".into())
            };
        }
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
        let expected_content_event = match candidate.entity_type.as_str() {
            "File" => "StreamUpdated",
            "FileVersion" => "Create",
            _ => return Err("stream backfill supports only the explicit TemperFS contract".into()),
        };
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
        let facts = historical_stream_facts(&candidate.entity_type, content_event)?;
        if facts.content_hash != candidate.content_hash
            || facts.byte_length != candidate.byte_length
            || candidate.content_type.as_deref() != facts.content_type.as_deref()
        {
            return Err("historical event content facts differ from inventory".into());
        }
        let expected_length = usize::try_from(candidate.byte_length)
            .map_err(|_| "historical stream length exceeds platform size".to_string())?;
        let bytes = match self
            .get_blob_with_legacy_fallback_bounded(
                tenant,
                &candidate.storage_object_id,
                expected_length,
            )
            .await?
        {
            BlobReadBounded::Found(bytes) => bytes,
            BlobReadBounded::Missing => return Err("historical stream blob is missing".into()),
            BlobReadBounded::TooLarge { .. } => {
                return Err("historical stream blob exceeds inventoried length".into());
            }
        };
        if bytes.len() != expected_length {
            return Err("historical stream blob length differs from inventory".into());
        }
        let actual_hash = format!("sha256:{:x}", Sha256::digest(&bytes));
        if actual_hash != candidate.content_hash {
            return Err("historical stream blob digest differs from inventory".into());
        }
        let descriptor_sequence = candidate
            .expected_current_sequence
            .checked_add(1)
            .ok_or_else(|| "historical stream sequence overflowed".to_string())?;
        let descriptor = StreamDescriptorV1::new(
            StreamEntityRef::new(&candidate.entity_type, &candidate.entity_id)
                .map_err(|error| error.to_string())?,
            facts.authorization_parent,
            &candidate.content_hash,
            StreamStorageRefV1::new(&candidate.storage_object_id)
                .map_err(|error| error.to_string())?,
            candidate.byte_length,
            candidate.content_type.clone(),
            candidate.content_event_sequence,
            descriptor_sequence,
            candidate.mutability,
        )
        .map_err(|error| error.to_string())?;
        self.validate_stream_descriptor_capability(tenant, None, &descriptor)?;
        let state = self
            .get_tenant_entity_state(tenant, &candidate.entity_type, &candidate.entity_id)
            .await?;
        if state.state.sequence_nr != candidate.expected_current_sequence {
            return Err("historical stream sequence changed before backfill commit".into());
        }
        let event = EntityEvent {
            action: STREAM_DESCRIPTOR_BACKFILLED_EVENT.into(),
            from_status: state.state.status.clone(),
            to_status: state.state.status,
            timestamp: sim_now(),
            params: serde_json::json!({}),
            idempotency_key: None,
        };
        let envelope = PersistenceEnvelope {
            sequence_nr: descriptor_sequence,
            event_type: STREAM_DESCRIPTOR_BACKFILLED_EVENT.into(),
            payload: serde_json::to_value(event).map_err(|error| error.to_string())?,
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
            .map_err(|error| error.to_string())?;
        self.stop_and_remove_entity(tenant, &candidate.entity_type, &candidate.entity_id);
        Ok(StreamDescriptorBackfillOutcomeV1::Appended {
            descriptor_event_sequence: descriptor_sequence,
        })
    }
}

struct HistoricalStreamFacts {
    content_hash: String,
    byte_length: u64,
    content_type: Option<String>,
    authorization_parent: Option<StreamEntityRef>,
}

fn historical_stream_facts(
    entity_type: &str,
    envelope: &PersistenceEnvelope,
) -> Result<HistoricalStreamFacts, String> {
    let event: EntityEvent = serde_json::from_value(envelope.payload.clone())
        .map_err(|error| format!("historical stream event is invalid: {error}"))?;
    if event.action != envelope.event_type {
        return Err("historical stream event action differs from its envelope".into());
    }
    let content_hash = event
        .params
        .get("content_hash")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "historical stream event has no exact content_hash".to_string())?
        .to_string();
    let byte_length = event
        .params
        .get("size_bytes")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            "historical stream event has no non-negative exact size_bytes".to_string()
        })?;
    let content_type = event
        .params
        .get("mime_type")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let authorization_parent = match entity_type {
        "File" => None,
        "FileVersion" => Some(
            StreamEntityRef::new(
                "File",
                event
                    .params
                    .get("file_id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        "historical FileVersion Create has no exact file_id".to_string()
                    })?,
            )
            .map_err(|error| error.to_string())?,
        ),
        _ => return Err("stream backfill supports only the explicit TemperFS contract".into()),
    };
    Ok(HistoricalStreamFacts {
        content_hash,
        byte_length,
        content_type,
        authorization_parent,
    })
}

pub(crate) async fn validate_backfill_replay_provenance(
    store: &BoxedEventStore,
    persistence_id: &str,
    descriptor: &StreamDescriptorV1,
) -> Result<(), String> {
    let content_sequence = descriptor.content_event_sequence();
    let descriptor_sequence = descriptor.descriptor_event_sequence();
    let content_events = store
        .read_events_limited(persistence_id, content_sequence.saturating_sub(1), 1)
        .await
        .map_err(|error| error.to_string())?;
    let [content_event] = content_events.as_slice() else {
        return Err("backfill content provenance event is unavailable".into());
    };
    if content_event.sequence_nr != content_sequence {
        return Err("backfill content provenance sequence is inconsistent".into());
    }
    let expected_event_type = match descriptor.subject().entity_type() {
        "File" => "StreamUpdated",
        "FileVersion" => "Create",
        _ => return Err("backfill descriptor is not an explicit TemperFS stream".into()),
    };
    if content_event.event_type != expected_event_type {
        return Err("backfill content provenance has the wrong event type".into());
    }
    let facts = historical_stream_facts(descriptor.subject().entity_type(), content_event)?;
    if facts.content_hash != descriptor.content_hash()
        || facts.byte_length != descriptor.byte_length()
        || facts.content_type.as_deref() != descriptor.content_type()
        || facts.authorization_parent.as_ref() != descriptor.authorization_parent()
    {
        return Err("backfill descriptor differs from historical event provenance".into());
    }
    let intermediate_count = descriptor_sequence
        .checked_sub(content_sequence)
        .and_then(|distance| distance.checked_sub(1))
        .ok_or_else(|| "backfill descriptor sequence ordering is invalid".to_string())?;
    let intermediate_count = usize::try_from(intermediate_count)
        .map_err(|_| "backfill provenance exceeds platform size".to_string())?;
    if intermediate_count > BACKFILL_HISTORY_EVENT_BUDGET {
        return Err("backfill provenance exceeds its replay budget".into());
    }
    let intermediate = store
        .read_events_limited(persistence_id, content_sequence, intermediate_count)
        .await
        .map_err(|error| error.to_string())?;
    if intermediate.len() != intermediate_count
        || intermediate.iter().enumerate().any(|(offset, event)| {
            u64::try_from(offset)
                .ok()
                .and_then(|offset| content_sequence.checked_add(offset))
                .and_then(|sequence| sequence.checked_add(1))
                != Some(event.sequence_nr)
                || event.event_type == expected_event_type
        })
    {
        return Err("backfill provenance does not cover the latest content publication".into());
    }
    Ok(())
}
