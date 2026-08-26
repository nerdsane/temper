//! Canonical stream-capability projection into generated artifacts.

use std::collections::BTreeSet;

use temper_spec::csdl::{
    CsdlDocument, StreamCapabilityMutabilityV1, verify_stream_capabilities_v1,
};
use temper_wasm_sdk::data::{ManifestStreamMutabilityV1, ModuleDataGrant, StreamCapabilityV1};

use super::ModuleSdkCodegenError;

pub(super) fn stream_capabilities_for_grant(
    csdl: &CsdlDocument,
    grant: &ModuleDataGrant,
) -> Result<Vec<StreamCapabilityV1>, ModuleSdkCodegenError> {
    let stream_subjects = grant
        .entities
        .iter()
        .filter(|entity| !entity.file_operations.is_empty())
        .map(|entity| entity.entity_type.as_str())
        .collect::<BTreeSet<_>>();
    if stream_subjects.is_empty() {
        return Ok(Vec::new());
    }
    let verified = verify_stream_capabilities_v1(csdl)
        .map_err(|error| ModuleSdkCodegenError::StreamCapability(error.to_string()))?;
    let mut included_types = stream_subjects
        .iter()
        .map(|value| (*value).to_string())
        .collect::<BTreeSet<_>>();
    for capability in &verified {
        if stream_subjects.contains(capability.subject_type.as_str())
            && let Some(version_type) = &capability.version_entity_type
        {
            included_types.insert(version_type.clone());
        }
    }
    let mut capabilities = verified
        .into_iter()
        .filter(|capability| included_types.contains(&capability.subject_type))
        .map(|capability| StreamCapabilityV1 {
            descriptor_contract_version: 1,
            subject_type: capability.subject_type,
            mutability: match capability.mutability {
                StreamCapabilityMutabilityV1::Mutable => ManifestStreamMutabilityV1::Mutable,
                StreamCapabilityMutabilityV1::Immutable => ManifestStreamMutabilityV1::Immutable,
            },
            version_entity_type: capability.version_entity_type,
            version_collection_navigation: capability.version_collection_navigation,
            authorization_parent_navigation: capability.authorization_parent_navigation,
            authorization_parent_type: capability.authorization_parent_type,
        })
        .collect::<Vec<_>>();
    capabilities.sort_by(|left, right| left.subject_type.cmp(&right.subject_type));
    Ok(capabilities)
}
