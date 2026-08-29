//! Durable, kernel-owned stream descriptor contracts.

use serde::{Deserialize, Serialize};

/// The stream descriptor contract version implemented by this runtime.
pub const STREAM_DESCRIPTOR_CONTRACT_V1: u16 = 1;

const MAX_ENTITY_TYPE_BYTES: usize = 512;
const MAX_ENTITY_ID_BYTES: usize = 512;
const MAX_CONTENT_HASH_BYTES: usize = 256;
const MAX_CONTENT_TYPE_BYTES: usize = 1_024;
const MAX_STORAGE_OBJECT_ID_BYTES: usize = 2_048;

/// A validation failure for kernel stream metadata.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StreamDescriptorError {
    /// A required bounded string was empty, padded, or too large.
    #[error("{field} must contain 1..={max_bytes} UTF-8 bytes without surrounding whitespace")]
    InvalidBoundedString {
        /// Stable field identity.
        field: &'static str,
        /// Maximum encoded UTF-8 length.
        max_bytes: usize,
    },
    /// An optional bounded string exceeded its byte budget.
    #[error("{field} must contain at most {max_bytes} UTF-8 bytes")]
    OptionalStringTooLong {
        /// Stable field identity.
        field: &'static str,
        /// Maximum encoded UTF-8 length.
        max_bytes: usize,
    },
    /// The storage contract version is not supported.
    #[error("unsupported stream storage contract version {0}")]
    UnsupportedStorageContract(u16),
    /// A durable event sequence was zero or ordered incorrectly.
    #[error(
        "stream descriptor sequences require 0 < content_event_sequence <= descriptor_event_sequence"
    )]
    InvalidEventSequences,
}

/// The entity identity governed by a stream descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "UncheckedStreamEntityRef")]
pub struct StreamEntityRef {
    entity_type: String,
    entity_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UncheckedStreamEntityRef {
    entity_type: String,
    entity_id: String,
}

impl StreamEntityRef {
    /// Construct a bounded canonical entity reference.
    pub fn new(
        entity_type: impl Into<String>,
        entity_id: impl Into<String>,
    ) -> Result<Self, StreamDescriptorError> {
        let entity_type = entity_type.into();
        let entity_id = entity_id.into();
        validate_required("subject.entity_type", &entity_type, MAX_ENTITY_TYPE_BYTES)?;
        validate_required("subject.entity_id", &entity_id, MAX_ENTITY_ID_BYTES)?;
        Ok(Self {
            entity_type,
            entity_id,
        })
    }

    /// Fully qualified entity type.
    pub fn entity_type(&self) -> &str {
        &self.entity_type
    }

    /// Entity identifier within the tenant authority.
    pub fn entity_id(&self) -> &str {
        &self.entity_id
    }
}

impl TryFrom<UncheckedStreamEntityRef> for StreamEntityRef {
    type Error = StreamDescriptorError;

    fn try_from(value: UncheckedStreamEntityRef) -> Result<Self, Self::Error> {
        Self::new(value.entity_type, value.entity_id)
    }
}

/// A bounded, provider-opaque blob identity persisted by the kernel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "UncheckedStreamStorageRefV1")]
pub struct StreamStorageRefV1 {
    contract_version: u16,
    object_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UncheckedStreamStorageRefV1 {
    contract_version: u16,
    object_id: String,
}

impl StreamStorageRefV1 {
    /// Construct an opaque storage reference minted by a V1 blob boundary.
    pub fn new(object_id: impl Into<String>) -> Result<Self, StreamDescriptorError> {
        let object_id = object_id.into();
        validate_required("storage.object_id", &object_id, MAX_STORAGE_OBJECT_ID_BYTES)?;
        Ok(Self {
            contract_version: STREAM_DESCRIPTOR_CONTRACT_V1,
            object_id,
        })
    }

    /// Storage-reference contract version.
    pub fn contract_version(&self) -> u16 {
        self.contract_version
    }

    /// Provider-opaque object identity.
    pub fn object_id(&self) -> &str {
        &self.object_id
    }
}

impl TryFrom<UncheckedStreamStorageRefV1> for StreamStorageRefV1 {
    type Error = StreamDescriptorError;

    fn try_from(value: UncheckedStreamStorageRefV1) -> Result<Self, Self::Error> {
        if value.contract_version != STREAM_DESCRIPTOR_CONTRACT_V1 {
            return Err(StreamDescriptorError::UnsupportedStorageContract(
                value.contract_version,
            ));
        }
        Self::new(value.object_id)
    }
}

/// Whether a verified stream may be replaced by a later content commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamMutability {
    /// A later verified content event may replace the descriptor.
    Mutable,
    /// The first descriptor is permanent for this subject.
    Immutable,
}

/// Authoritative stream identity, ownership, storage, and admission metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "UncheckedStreamDescriptorV1")]
pub struct StreamDescriptorV1 {
    subject: StreamEntityRef,
    authorization_parent: Option<StreamEntityRef>,
    content_hash: String,
    storage: StreamStorageRefV1,
    byte_length: u64,
    content_type: Option<String>,
    content_event_sequence: u64,
    descriptor_event_sequence: u64,
    mutability: StreamMutability,
}

/// Named inputs used to construct and validate a V1 stream descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamDescriptorInputV1 {
    /// Entity whose `$value` this descriptor governs.
    pub subject: StreamEntityRef,
    /// Optional verified parent used for authorization.
    pub authorization_parent: Option<StreamEntityRef>,
    /// Platform-computed content digest.
    pub content_hash: String,
    /// Persisted opaque storage identity.
    pub storage: StreamStorageRefV1,
    /// Host-attested accepted byte count.
    pub byte_length: u64,
    /// Optional media type committed with the content.
    pub content_type: Option<String>,
    /// Domain event that published the content.
    pub content_event_sequence: u64,
    /// Event envelope that persisted this descriptor.
    pub descriptor_event_sequence: u64,
    /// Verified replacement semantics.
    pub mutability: StreamMutability,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UncheckedStreamDescriptorV1 {
    subject: StreamEntityRef,
    authorization_parent: Option<StreamEntityRef>,
    content_hash: String,
    storage: StreamStorageRefV1,
    byte_length: u64,
    content_type: Option<String>,
    content_event_sequence: u64,
    descriptor_event_sequence: u64,
    mutability: StreamMutability,
}

impl StreamDescriptorV1 {
    /// Construct a validated descriptor for a normal commit or audited backfill.
    pub fn new(input: StreamDescriptorInputV1) -> Result<Self, StreamDescriptorError> {
        let StreamDescriptorInputV1 {
            subject,
            authorization_parent,
            content_hash,
            storage,
            byte_length,
            content_type,
            content_event_sequence,
            descriptor_event_sequence,
            mutability,
        } = input;
        validate_required("content_hash", &content_hash, MAX_CONTENT_HASH_BYTES)?;
        if content_type
            .as_ref()
            .is_some_and(|value| value.len() > MAX_CONTENT_TYPE_BYTES)
        {
            return Err(StreamDescriptorError::OptionalStringTooLong {
                field: "content_type",
                max_bytes: MAX_CONTENT_TYPE_BYTES,
            });
        }
        if content_event_sequence == 0 || content_event_sequence > descriptor_event_sequence {
            return Err(StreamDescriptorError::InvalidEventSequences);
        }
        Ok(Self {
            subject,
            authorization_parent,
            content_hash,
            storage,
            byte_length,
            content_type,
            content_event_sequence,
            descriptor_event_sequence,
            mutability,
        })
    }

    /// Entity whose `$value` this descriptor governs.
    pub fn subject(&self) -> &StreamEntityRef {
        &self.subject
    }

    /// Optional verified parent used for authorization.
    pub fn authorization_parent(&self) -> Option<&StreamEntityRef> {
        self.authorization_parent.as_ref()
    }

    /// Platform-computed content digest.
    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }

    /// Persisted opaque storage identity.
    pub fn storage(&self) -> &StreamStorageRefV1 {
        &self.storage
    }

    /// Host-attested accepted byte count.
    pub fn byte_length(&self) -> u64 {
        self.byte_length
    }

    /// Optional media type committed with the content.
    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    /// Domain event that published the content.
    pub fn content_event_sequence(&self) -> u64 {
        self.content_event_sequence
    }

    /// Event envelope that persisted this descriptor.
    pub fn descriptor_event_sequence(&self) -> u64 {
        self.descriptor_event_sequence
    }

    /// Verified replacement semantics.
    pub fn mutability(&self) -> StreamMutability {
        self.mutability
    }

    /// Whether this descriptor was attached by an audited later backfill.
    pub fn is_backfill(&self) -> bool {
        self.content_event_sequence < self.descriptor_event_sequence
    }
}

impl TryFrom<UncheckedStreamDescriptorV1> for StreamDescriptorV1 {
    type Error = StreamDescriptorError;

    fn try_from(value: UncheckedStreamDescriptorV1) -> Result<Self, Self::Error> {
        Self::new(StreamDescriptorInputV1 {
            subject: value.subject,
            authorization_parent: value.authorization_parent,
            content_hash: value.content_hash,
            storage: value.storage,
            byte_length: value.byte_length,
            content_type: value.content_type,
            content_event_sequence: value.content_event_sequence,
            descriptor_event_sequence: value.descriptor_event_sequence,
            mutability: value.mutability,
        })
    }
}

/// Closed, version-tagged kernel metadata carried by an event envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "version", deny_unknown_fields)]
pub enum KernelEventMetadata {
    /// Durable stream descriptor contract V1.
    #[serde(rename = "1")]
    V1 {
        /// Descriptor minted or backfilled by this event.
        stream_descriptor: StreamDescriptorV1,
    },
}

impl KernelEventMetadata {
    /// Return the descriptor carried by this metadata version.
    pub fn stream_descriptor(&self) -> &StreamDescriptorV1 {
        match self {
            Self::V1 { stream_descriptor } => stream_descriptor,
        }
    }
}

fn validate_required(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), StreamDescriptorError> {
    if value.is_empty() || value.trim() != value || value.len() > max_bytes {
        return Err(StreamDescriptorError::InvalidBoundedString { field, max_bytes });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor() -> StreamDescriptorV1 {
        StreamDescriptorV1::new(StreamDescriptorInputV1 {
            subject: StreamEntityRef::new("Temper.FS.File", "file-1").unwrap(),
            authorization_parent: None,
            content_hash: "sha256:abc".into(),
            storage: StreamStorageRefV1::new("objects/ab/cd").unwrap(),
            byte_length: 0,
            content_type: Some("application/octet-stream".into()),
            content_event_sequence: 7,
            descriptor_event_sequence: 7,
            mutability: StreamMutability::Mutable,
        })
        .unwrap()
    }

    #[test]
    fn descriptor_round_trips_closed_v1_encoding() {
        let metadata = KernelEventMetadata::V1 {
            stream_descriptor: descriptor(),
        };
        let json = serde_json::to_value(&metadata).unwrap();
        assert_eq!(json["version"], "1");
        assert_eq!(json["stream_descriptor"]["byte_length"], 0);
        assert_eq!(
            serde_json::from_value::<KernelEventMetadata>(json).unwrap(),
            metadata
        );
    }

    #[test]
    fn descriptor_decode_rejects_unknown_fields_and_versions() {
        let mut value = serde_json::to_value(KernelEventMetadata::V1 {
            stream_descriptor: descriptor(),
        })
        .unwrap();
        value["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<KernelEventMetadata>(value).is_err());
        assert!(
            serde_json::from_value::<KernelEventMetadata>(serde_json::json!({
                "version": "2",
                "stream_descriptor": serde_json::to_value(descriptor()).unwrap()
            }))
            .is_err()
        );
    }

    #[test]
    fn descriptor_decode_enforces_bounds_and_sequence_order() {
        let mut value = serde_json::to_value(descriptor()).unwrap();
        value["content_hash"] = serde_json::Value::String("x".repeat(257));
        assert!(serde_json::from_value::<StreamDescriptorV1>(value).is_err());

        let mut value = serde_json::to_value(descriptor()).unwrap();
        value["content_event_sequence"] = serde_json::json!(8);
        assert!(serde_json::from_value::<StreamDescriptorV1>(value).is_err());
    }

    #[test]
    fn historical_event_metadata_decodes_without_kernel_member() {
        let value = serde_json::json!({
            "event_id": "00000000-0000-0000-0000-000000000000",
            "causation_id": "00000000-0000-0000-0000-000000000000",
            "correlation_id": "00000000-0000-0000-0000-000000000000",
            "timestamp": "2026-08-26T00:00:00Z",
            "actor_id": "default:File:file-1"
        });
        let metadata: crate::persistence::EventMetadata = serde_json::from_value(value).unwrap();
        assert!(metadata.kernel.is_none());
    }
}
