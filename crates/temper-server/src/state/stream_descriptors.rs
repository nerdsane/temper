use sha2::{Digest as _, Sha256};
use temper_runtime::persistence::{StreamDescriptorV1, StreamMutability};
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::{StreamCapabilityMutabilityV1, verify_stream_capabilities_v1};

use crate::blob_store::BlobReadBounded;

use super::ServerState;

const DESCRIPTOR_REPLAY_EVENT_BUDGET: usize = 1_024;

fn active_stream_publication_binding(
    config: &crate::registry::TenantConfig,
    entity_type: &str,
) -> Option<(String, String)> {
    let capabilities = verify_stream_capabilities_v1(&config.csdl).ok()?;
    let mut matches = capabilities
        .iter()
        .filter(|capability| capability.subject_type.rsplit('.').next() == Some(entity_type));
    let capability = matches.next()?;
    if matches.next().is_some() || !capability.descriptor_contract_v1_active {
        return None;
    }
    let publication_action = capability
        .migration_provenance
        .as_ref()?
        .publication_action
        .clone();
    let capability_digest =
        temper_spec::csdl::stream_capability_set_digest_v1(std::slice::from_ref(capability))
            .ok()?;
    Some((publication_action, capability_digest))
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum StreamDescriptorResolutionError {
    #[error("event journal is unavailable")]
    JournalUnavailable,
    #[error("stream descriptor is missing")]
    Missing,
    #[error("stream descriptor replay exceeded its event budget")]
    ReplayBudgetExceeded,
    #[error("stream descriptor is inconsistent: {0}")]
    Consistency(String),
    #[error("stream content exceeds the invocation budget")]
    BudgetExceeded,
    #[error("stream storage failed: {0}")]
    Storage(String),
    #[error("stream content integrity verification failed: {0}")]
    Integrity(String),
}

impl StreamDescriptorResolutionError {
    pub(crate) const fn stable_code(&self) -> &'static str {
        match self {
            Self::BudgetExceeded => "FileSizeBudgetExceeded",
            Self::Missing => "StreamDescriptorMissing",
            Self::Integrity(_) => "StreamIntegrityMismatch",
            Self::ReplayBudgetExceeded | Self::Consistency(_) => "StreamDescriptorInconsistent",
            Self::JournalUnavailable | Self::Storage(_) => "StreamDescriptorUnavailable",
        }
    }
}

pub(crate) struct ResolvedStreamContent {
    pub(crate) descriptor: StreamDescriptorV1,
    pub(crate) bytes: Vec<u8>,
}

impl ServerState {
    pub(crate) async fn stream_descriptor_contract_activated(
        &self,
        tenant: &TenantId,
        schema_pin: Option<&temper_runtime::persistence::schema_deployment::SchemaExecutionPin>,
        entity_type: &str,
    ) -> Result<bool, StreamDescriptorResolutionError> {
        let declared_binding = {
            let registry = self
                .registry
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let config = match schema_pin {
                Some(pin) => {
                    registry.get_scoped_config_at_digest(tenant, &pin.scope, &pin.bundle_digest)
                }
                None => registry.get_tenant(tenant),
            };
            config.and_then(|config| active_stream_publication_binding(config, entity_type))
        };
        if let Some(pin) = schema_pin {
            if declared_binding.is_some() {
                return Ok(true);
            }
            let Some(store) = self.schema_deployment_store() else {
                return Err(StreamDescriptorResolutionError::JournalUnavailable);
            };
            let Some(pointer) = store
                .active_schema_pointer(tenant.as_str(), &pin.scope)
                .await
                .map_err(|error| StreamDescriptorResolutionError::Storage(error.to_string()))?
            else {
                return Ok(false);
            };
            if pointer.predecessor_digest.as_deref() != Some(pin.bundle_digest.as_str())
                || !pointer
                    .stream_publication_bindings
                    .contains_key(entity_type)
            {
                return Ok(false);
            }
            let registry = self
                .registry
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            return Ok(registry
                .get_scoped_config_at_digest(tenant, &pin.scope, &pointer.bundle_digest)
                .and_then(|config| active_stream_publication_binding(config, entity_type))
                .is_some());
        }
        let Some((publication_action, capability_digest)) = declared_binding else {
            return Ok(false);
        };
        let Some((journal, _)) = self.event_journal() else {
            return Err(StreamDescriptorResolutionError::JournalUnavailable);
        };
        journal
            .unscoped_stream_publication_fence_active(
                tenant.as_str(),
                entity_type,
                &publication_action,
                &capability_digest,
            )
            .await
            .map_err(|error| StreamDescriptorResolutionError::Storage(error.to_string()))
    }

    pub(crate) fn validate_stream_descriptor_capability(
        &self,
        tenant: &TenantId,
        schema_pin: Option<&temper_runtime::persistence::schema_deployment::SchemaExecutionPin>,
        descriptor: &StreamDescriptorV1,
    ) -> Result<(), String> {
        let registry = self
            .registry
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = match schema_pin {
            Some(pin) => {
                registry.get_scoped_config_at_digest(tenant, &pin.scope, &pin.bundle_digest)
            }
            None => registry.get_tenant(tenant),
        }
        .ok_or_else(|| "stream descriptor schema is unavailable".to_string())?;
        let capabilities = verify_stream_capabilities_v1(&config.csdl)
            .map_err(|error| format!("stream descriptor schema is invalid: {error}"))?;
        let matches: Vec<_> = capabilities
            .iter()
            .filter(|capability| {
                capability.subject_type.rsplit('.').next()
                    == Some(descriptor.subject().entity_type())
            })
            .collect();
        if matches.len() != 1 {
            return Err("stream descriptor subject capability is missing or ambiguous".into());
        }
        let capability = matches[0];
        let mutability_matches = matches!(
            (capability.mutability, descriptor.mutability()),
            (
                StreamCapabilityMutabilityV1::Mutable,
                StreamMutability::Mutable
            ) | (
                StreamCapabilityMutabilityV1::Immutable,
                StreamMutability::Immutable
            )
        );
        let parent_matches = match (
            capability.authorization_parent_type.as_deref(),
            descriptor.authorization_parent(),
        ) {
            (None, None) => true,
            (Some(expected), Some(actual)) => {
                expected.rsplit('.').next() == Some(actual.entity_type())
            }
            _ => false,
        };
        if !mutability_matches || !parent_matches {
            return Err("stream descriptor relation or mutability differs from CSDL".into());
        }
        Ok(())
    }

    #[tracing::instrument(skip_all, fields(
        otel.name = "state.resolve_stream_descriptor",
        entity_type,
        descriptor_contract_version = 1_u64,
        resolution_source = "journal",
        sequence_agreement = tracing::field::Empty,
    ))]
    pub(crate) async fn resolve_stream_descriptor(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<StreamDescriptorV1, StreamDescriptorResolutionError> {
        let (journal, _) = self
            .event_journal()
            .ok_or(StreamDescriptorResolutionError::JournalUnavailable)?;
        let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
        let events = journal
            .read_latest_events(&persistence_id, DESCRIPTOR_REPLAY_EVENT_BUDGET + 1)
            .await
            .map_err(|error| StreamDescriptorResolutionError::Storage(error.to_string()))?;
        let observed_tail = events.last().map_or(0, |event| event.sequence_nr);
        let tail_probe = journal
            .read_events_limited(&persistence_id, observed_tail, 1)
            .await
            .map_err(|error| StreamDescriptorResolutionError::Storage(error.to_string()))?;
        if !tail_probe.is_empty() {
            return Err(StreamDescriptorResolutionError::Consistency(
                "journal read did not reach a stable tail".into(),
            ));
        }
        let exhausted = events.len() > DESCRIPTOR_REPLAY_EVENT_BUDGET;
        let descriptor = resolve_descriptor_events(entity_type, entity_id, &events, exhausted)?;
        tracing::Span::current().record(
            "sequence_agreement",
            descriptor.content_event_sequence() <= descriptor.descriptor_event_sequence(),
        );
        Ok(descriptor)
    }

    #[tracing::instrument(skip_all, fields(
        otel.name = "state.open_stream_from_descriptor",
        entity_type,
        budget_bytes,
        declared_byte_length = tracing::field::Empty,
        budget_outcome = tracing::field::Empty,
        blob_fetch_began = false,
    ))]
    pub(crate) async fn open_stream_from_descriptor(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        budget_bytes: u64,
    ) -> Result<ResolvedStreamContent, StreamDescriptorResolutionError> {
        let descriptor = self
            .resolve_stream_descriptor(tenant, entity_type, entity_id)
            .await?;
        self.validate_stream_descriptor_capability(tenant, None, &descriptor)
            .map_err(StreamDescriptorResolutionError::Consistency)?;
        let bytes = self
            .read_stream_descriptor_bytes(tenant, &descriptor, budget_bytes)
            .await?;
        Ok(ResolvedStreamContent { descriptor, bytes })
    }

    /// Enforce admission and fetch bytes for an already-authorized descriptor.
    #[tracing::instrument(skip_all, fields(
        otel.name = "state.read_stream_descriptor_bytes",
        entity_type = descriptor.subject().entity_type(),
        budget_bytes,
        declared_byte_length = tracing::field::Empty,
        budget_outcome = tracing::field::Empty,
        blob_fetch_began = false,
    ))]
    pub(crate) async fn read_stream_descriptor_bytes(
        &self,
        tenant: &TenantId,
        descriptor: &StreamDescriptorV1,
        budget_bytes: u64,
    ) -> Result<Vec<u8>, StreamDescriptorResolutionError> {
        tracing::Span::current().record("declared_byte_length", descriptor.byte_length());
        if descriptor.byte_length() > budget_bytes {
            tracing::Span::current().record("budget_outcome", "rejected");
            return Err(StreamDescriptorResolutionError::BudgetExceeded);
        }
        let expected_length = usize::try_from(descriptor.byte_length()).map_err(|_| {
            StreamDescriptorResolutionError::Consistency(
                "descriptor byte length exceeds platform size".into(),
            )
        })?;
        tracing::Span::current().record("budget_outcome", "accepted");
        tracing::Span::current().record("blob_fetch_began", true);
        let bytes = match self
            .get_blob_with_legacy_fallback_bounded(
                tenant,
                descriptor.storage().object_id(),
                expected_length,
            )
            .await
            .map_err(StreamDescriptorResolutionError::Storage)?
        {
            BlobReadBounded::Found(bytes) => bytes,
            BlobReadBounded::Missing => {
                return Err(StreamDescriptorResolutionError::Integrity(
                    "persisted storage reference is missing".into(),
                ));
            }
            BlobReadBounded::TooLarge { .. } => {
                return Err(StreamDescriptorResolutionError::Integrity(
                    "stored content is longer than its descriptor".into(),
                ));
            }
        };
        if bytes.len() != expected_length {
            return Err(StreamDescriptorResolutionError::Integrity(
                "stored content length differs from its descriptor".into(),
            ));
        }
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let actual_hash = format!("sha256:{:x}", hasher.finalize());
        if actual_hash != descriptor.content_hash() {
            return Err(StreamDescriptorResolutionError::Integrity(
                "stored content digest differs from its descriptor".into(),
            ));
        }
        Ok(bytes)
    }
}

fn resolve_descriptor_events(
    entity_type: &str,
    entity_id: &str,
    events: &[temper_runtime::persistence::PersistenceEnvelope],
    exhausted: bool,
) -> Result<StreamDescriptorV1, StreamDescriptorResolutionError> {
    let mut resolved: Option<StreamDescriptorV1> = None;
    for event in events {
        let Some(kernel) = event.metadata.kernel.as_ref() else {
            continue;
        };
        let descriptor = kernel.stream_descriptor();
        if descriptor.subject().entity_type() != entity_type
            || descriptor.subject().entity_id() != entity_id
            || descriptor.descriptor_event_sequence() != event.sequence_nr
            || descriptor.content_event_sequence() > event.sequence_nr
        {
            return Err(StreamDescriptorResolutionError::Consistency(
                "journal identity or event sequence does not match descriptor".into(),
            ));
        }
        if resolved
            .as_ref()
            .is_some_and(|prior| prior.mutability() == StreamMutability::Immutable)
        {
            return Err(StreamDescriptorResolutionError::Consistency(
                "immutable stream descriptor was replaced".into(),
            ));
        }
        resolved = Some(descriptor.clone());
    }
    match resolved {
        Some(descriptor) => Ok(descriptor),
        None if exhausted => Err(StreamDescriptorResolutionError::ReplayBudgetExceeded),
        None => Err(StreamDescriptorResolutionError::Missing),
    }
}

#[cfg(test)]
mod tests {
    use temper_runtime::ActorSystem;
    use temper_runtime::persistence::{
        EventMetadata, EventStore, KernelEventMetadata, PersistenceEnvelope,
        StreamDescriptorInputV1, StreamEntityRef, StreamStorageRefV1,
    };
    use temper_store_sim::{SimEventStore, SimFaultConfig};
    use uuid::Uuid;

    use super::*;

    fn event(sequence: u64, mutability: StreamMutability) -> PersistenceEnvelope {
        let descriptor = StreamDescriptorV1::new(StreamDescriptorInputV1 {
            subject: StreamEntityRef::new("File", "file-1").unwrap(),
            authorization_parent: None,
            content_hash: "sha256:abc".into(),
            storage: StreamStorageRefV1::new("streams/abc").unwrap(),
            byte_length: 3,
            content_type: Some("text/plain".into()),
            content_event_sequence: sequence,
            descriptor_event_sequence: sequence,
            mutability,
        })
        .unwrap();
        PersistenceEnvelope {
            sequence_nr: sequence,
            event_type: "StreamUpdated".into(),
            payload: serde_json::json!({}),
            metadata: EventMetadata {
                event_id: Uuid::nil(),
                causation_id: Uuid::nil(),
                correlation_id: Uuid::nil(),
                timestamp: temper_runtime::scheduler::sim_now(),
                actor_id: "default:File:file-1".into(),
                kernel: Some(KernelEventMetadata::V1 {
                    stream_descriptor: descriptor,
                }),
            },
        }
    }

    #[test]
    fn replay_uses_latest_mutable_descriptor_and_preserves_historical_absence() {
        assert_eq!(
            resolve_descriptor_events("File", "file-1", &[], false),
            Err(StreamDescriptorResolutionError::Missing)
        );
        let events = vec![
            event(1, StreamMutability::Mutable),
            event(2, StreamMutability::Mutable),
        ];
        assert_eq!(
            resolve_descriptor_events("File", "file-1", &events, false)
                .unwrap()
                .descriptor_event_sequence(),
            2
        );
    }

    #[test]
    fn replay_rejects_immutable_replacement_and_identity_drift() {
        let immutable = vec![
            event(1, StreamMutability::Immutable),
            event(2, StreamMutability::Immutable),
        ];
        assert!(matches!(
            resolve_descriptor_events("File", "file-1", &immutable, false),
            Err(StreamDescriptorResolutionError::Consistency(_))
        ));
        assert!(matches!(
            resolve_descriptor_events(
                "File",
                "another-file",
                &[event(1, StreamMutability::Mutable)],
                false
            ),
            Err(StreamDescriptorResolutionError::Consistency(_))
        ));
    }

    #[tokio::test]
    async fn successful_truncated_latest_read_fails_closed() {
        let sim = SimEventStore::no_faults(1_187);
        sim.append(
            "default:File:file-1",
            0,
            &[
                event(1, StreamMutability::Mutable),
                event(2, StreamMutability::Mutable),
            ],
        )
        .await
        .unwrap();
        sim.restore_faults(SimFaultConfig {
            read_truncation_prob: 1.0,
            ..SimFaultConfig::none()
        });
        let mut state = crate::ServerState::from_registry(
            ActorSystem::new("stream-descriptor-truncation"),
            crate::registry::SpecRegistry::new(),
        );
        state.set_storage_stack(crate::storage::StorageStack::from_sim(sim, None));
        assert!(matches!(
            state
                .resolve_stream_descriptor(&TenantId::default(), "File", "file-1")
                .await,
            Err(StreamDescriptorResolutionError::Consistency(_))
        ));
    }

    #[tokio::test]
    async fn budget_precedes_blob_access_and_integrity_is_fail_closed() {
        let mut state = crate::ServerState::from_registry(
            ActorSystem::new("stream-descriptor-integrity"),
            crate::registry::SpecRegistry::new(),
        );
        let data_dir = tempfile::tempdir().unwrap();
        state.data_dir = data_dir.path().to_path_buf();
        let tenant = TenantId::default();
        let receipt = state
            .put_stream_content_attested(&tenant, "temper-fs/", b"abc", Some("text/plain"))
            .await
            .unwrap();
        let descriptor = receipt
            .into_descriptor(
                StreamEntityRef::new("File", "file-1").unwrap(),
                None,
                1,
                StreamMutability::Mutable,
            )
            .unwrap();
        assert_eq!(
            state
                .read_stream_descriptor_bytes(&tenant, &descriptor, 2)
                .await,
            Err(StreamDescriptorResolutionError::BudgetExceeded)
        );
        assert_eq!(
            state
                .read_stream_descriptor_bytes(&tenant, &descriptor, 3)
                .await
                .unwrap(),
            b"abc"
        );
        let corrupt_digest = StreamDescriptorV1::new(StreamDescriptorInputV1 {
            subject: descriptor.subject().clone(),
            authorization_parent: None,
            content_hash: "sha256:wrong".into(),
            storage: descriptor.storage().clone(),
            byte_length: descriptor.byte_length(),
            content_type: descriptor.content_type().map(str::to_string),
            content_event_sequence: 1,
            descriptor_event_sequence: 1,
            mutability: StreamMutability::Mutable,
        })
        .unwrap();
        assert!(matches!(
            state
                .read_stream_descriptor_bytes(&tenant, &corrupt_digest, 3)
                .await,
            Err(StreamDescriptorResolutionError::Integrity(_))
        ));
    }

    #[tokio::test]
    async fn zero_length_stream_succeeds_at_zero_budget() {
        let mut state = crate::ServerState::from_registry(
            ActorSystem::new("stream-descriptor-empty"),
            crate::registry::SpecRegistry::new(),
        );
        let data_dir = tempfile::tempdir().unwrap();
        state.data_dir = data_dir.path().to_path_buf();
        let tenant = TenantId::default();
        let descriptor = state
            .put_stream_content_attested(&tenant, "temper-fs/", b"", None)
            .await
            .unwrap()
            .into_descriptor(
                StreamEntityRef::new("File", "empty").unwrap(),
                None,
                1,
                StreamMutability::Mutable,
            )
            .unwrap();
        assert!(
            state
                .read_stream_descriptor_bytes(&tenant, &descriptor, 0)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
