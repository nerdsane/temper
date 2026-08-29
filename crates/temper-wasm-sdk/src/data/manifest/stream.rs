//! Closed stream semantics bound into generated artifacts.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Closed stream replacement semantics bound into a generated artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestStreamMutabilityV1 {
    /// A later verified content commit may replace the descriptor.
    Mutable,
    /// The first descriptor is permanent for this subject.
    Immutable,
}

/// Canonical stream semantics covered by the artifact binding digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamCapabilityV1 {
    /// Durable descriptor contract required from the host.
    pub descriptor_contract_version: u16,
    /// Fully qualified stream subject type.
    pub subject_type: String,
    /// Verified replacement semantics.
    pub mutability: ManifestStreamMutabilityV1,
    /// Fully qualified immutable version type, when present.
    pub version_entity_type: Option<String>,
    /// Canonical collection navigation from current entity to versions.
    pub version_collection_navigation: Option<String>,
    /// Canonical navigation from an immutable entity to its authorization parent.
    pub authorization_parent_navigation: Option<String>,
    /// Fully qualified authorization-parent type.
    pub authorization_parent_type: Option<String>,
}

pub(super) fn validate_stream_capabilities(
    capabilities: &[StreamCapabilityV1],
) -> Result<(), String> {
    let mut stream_types = BTreeSet::new();
    for capability in capabilities {
        if capability.descriptor_contract_version != 1 {
            return Err(format!(
                "unsupported stream descriptor contract version {}",
                capability.descriptor_contract_version
            ));
        }
        if !stream_types.insert(capability.subject_type.as_str()) {
            return Err(format!(
                "duplicate stream capability '{}'",
                capability.subject_type
            ));
        }
    }
    if !capabilities
        .windows(2)
        .all(|pair| pair[0].subject_type < pair[1].subject_type)
    {
        return Err("stream capabilities are not in canonical subject-type order".into());
    }
    Ok(())
}

impl super::ModuleSdkManifest {
    /// Digest of the exact verified stream semantics, when the artifact uses streams.
    pub fn stream_capabilities_digest(&self) -> Result<Option<String>, String> {
        if self.stream_capabilities.is_empty() {
            return Ok(None);
        }
        super::digest_json(&self.stream_capabilities).map(Some)
    }
}
