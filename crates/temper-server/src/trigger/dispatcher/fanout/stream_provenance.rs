use temper_runtime::persistence::{
    KernelEventMetadata, StreamDescriptorInputV1, StreamDescriptorV1, StreamEntityRef,
    StreamMutability,
};
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::{StreamCapabilityMutabilityV1, verify_stream_capabilities_v1};

use super::super::BoundDelivery;
use crate::trigger::types::{ReactionFailureKind, ReactionResult, ReactionRule};

pub(super) fn stream_provenance_failure(
    rule_name: &str,
    error: String,
    depth: u32,
) -> ReactionResult {
    ReactionResult {
        rule_name: rule_name.to_string(),
        success: false,
        target_status: None,
        error: Some(error),
        failure: Some(ReactionFailureKind::DispatchConflict),
        decision_id: None,
        depth,
    }
}

pub(super) fn immutable_version_metadata(
    state: &crate::ServerState,
    tenant: &TenantId,
    schema_pin: Option<&temper_runtime::persistence::schema_deployment::SchemaExecutionPin>,
    rule: &ReactionRule,
    target_entity_id: &str,
    target_current_sequence: u64,
    delivery: Option<&BoundDelivery>,
) -> Result<Option<KernelEventMetadata>, String> {
    let Some(source) = delivery.and_then(|value| value.source_stream_descriptor.as_ref()) else {
        return Ok(None);
    };
    if !matches!(
        rule.resolve_target,
        crate::trigger::types::TargetResolver::Create
    ) {
        return Ok(None);
    }

    let registry = state
        .registry
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let config = match schema_pin {
        Some(pin) => registry.get_scoped_config_at_digest(tenant, &pin.scope, &pin.bundle_digest),
        None => registry.get_tenant(tenant),
    }
    .ok_or_else(|| "stream version reaction schema is unavailable".to_string())?;
    let capabilities = verify_stream_capabilities_v1(&config.csdl)
        .map_err(|error| format!("stream version reaction schema is invalid: {error}"))?;
    let matching: Vec<_> = capabilities
        .iter()
        .filter(|capability| {
            capability.mutability == StreamCapabilityMutabilityV1::Mutable
                && entity_short_name(&capability.subject_type)
                    == Some(rule.when.entity_type.as_str())
                && capability
                    .version_entity_type
                    .as_deref()
                    .and_then(entity_short_name)
                    == Some(rule.then.entity_type.as_str())
        })
        .collect();
    if matching.is_empty() {
        let targets_immutable_stream = capabilities.iter().any(|capability| {
            capability.mutability == StreamCapabilityMutabilityV1::Immutable
                && entity_short_name(&capability.subject_type)
                    == Some(rule.then.entity_type.as_str())
        });
        return if targets_immutable_stream {
            Err("immutable stream reaction lacks a unique verified parent contract".into())
        } else {
            Ok(None)
        };
    }
    if matching.len() != 1 {
        return Err("stream version reaction capability is ambiguous".into());
    }
    if source.subject().entity_type() != rule.when.entity_type
        || source.mutability() != StreamMutability::Mutable
    {
        return Err(
            "source stream descriptor does not match the verified reaction contract".into(),
        );
    }
    let target_sequence = target_current_sequence
        .checked_add(1)
        .ok_or_else(|| "immutable stream target sequence overflowed".to_string())?;
    let descriptor = StreamDescriptorV1::new(StreamDescriptorInputV1 {
        subject: StreamEntityRef::new(&rule.then.entity_type, target_entity_id)
            .map_err(|error| error.to_string())?,
        authorization_parent: Some(source.subject().clone()),
        content_hash: source.content_hash().to_string(),
        storage: source.storage().clone(),
        byte_length: source.byte_length(),
        content_type: source.content_type().map(str::to_string),
        content_event_sequence: target_sequence,
        descriptor_event_sequence: target_sequence,
        mutability: StreamMutability::Immutable,
    })
    .map_err(|error| error.to_string())?;
    Ok(Some(KernelEventMetadata::V1 {
        stream_descriptor: descriptor,
    }))
}

fn entity_short_name(qualified: &str) -> Option<&str> {
    qualified.rsplit('.').next()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use temper_runtime::ActorSystem;
    use temper_runtime::persistence::{StreamMutability, StreamStorageRefV1};
    use temper_spec::csdl::parse_csdl;

    use super::*;
    use crate::registry::SpecRegistry;
    use crate::trigger::types::{ReactionTarget, ReactionTrigger, TargetResolver};

    #[test]
    fn provenance_rejection_is_a_typed_dispatch_conflict() {
        let result = stream_provenance_failure("version-stream", "contract mismatch".into(), 2);

        assert!(!result.success);
        assert_eq!(result.failure, Some(ReactionFailureKind::DispatchConflict));
        assert_eq!(result.decision_id, None);
        assert_eq!(result.depth, 2);
    }

    #[test]
    fn verified_create_reaction_mints_immutable_child_descriptor() {
        let csdl_xml = include_str!("../../../../../../os-apps/temper-fs/specs/model.csdl.xml");
        let mut registry = SpecRegistry::new();
        registry.register_tenant(
            "default",
            parse_csdl(csdl_xml).unwrap(),
            csdl_xml.to_string(),
            &[],
        );
        let state =
            crate::ServerState::from_registry(ActorSystem::new("stream-provenance-test"), registry);
        let source = StreamDescriptorV1::new(StreamDescriptorInputV1 {
            subject: StreamEntityRef::new("File", "file-1").unwrap(),
            authorization_parent: None,
            content_hash: "sha256:abc".into(),
            storage: StreamStorageRefV1::new("temper-fs/sha256:abc").unwrap(),
            byte_length: 3,
            content_type: Some("text/plain".into()),
            content_event_sequence: 2,
            descriptor_event_sequence: 2,
            mutability: StreamMutability::Mutable,
        })
        .unwrap();
        let delivery = BoundDelivery {
            delivery_id: "delivery-1".into(),
            root_delivery_id: "delivery-1".into(),
            fencing_token: 1,
            target_entity_id: Some("version-1".into()),
            expected_target_sequence: Some(0),
            state_timeout_state: None,
            collection: None,
            source_stream_descriptor: Some(source),
        };
        let rule = ReactionRule {
            name: "file_stream_updated_creates_version".into(),
            when: ReactionTrigger {
                entity_type: "File".into(),
                action: Some("StreamUpdated".into()),
                to_state: Some("Ready".into()),
                guard: None,
            },
            then: ReactionTarget {
                entity_type: "FileVersion".into(),
                action: "Create".into(),
                params: serde_json::json!({}),
                params_from: BTreeMap::new(),
            },
            resolve_target: TargetResolver::Create,
            principal: Some("file-service".into()),
            drop_ok: false,
        };
        let metadata = immutable_version_metadata(
            &state,
            &TenantId::default(),
            None,
            &rule,
            "version-1",
            0,
            Some(&delivery),
        )
        .unwrap()
        .expect("verified version reaction carries descriptor");
        let descriptor = metadata.stream_descriptor();
        assert_eq!(descriptor.subject().entity_type(), "FileVersion");
        assert_eq!(descriptor.subject().entity_id(), "version-1");
        assert_eq!(descriptor.mutability(), StreamMutability::Immutable);
        assert_eq!(descriptor.descriptor_event_sequence(), 1);
        assert_eq!(
            descriptor.authorization_parent(),
            Some(&StreamEntityRef::new("File", "file-1").unwrap())
        );
    }
}
