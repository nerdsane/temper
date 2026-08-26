//! Redacted observation for typed application failures.

use sha2::{Digest, Sha256};
use temper_failure::{
    CausalOperationV1, FailureCategory, FailureContractError, FailureEnvelopeV1, OperationAttempt,
    OperationId, OperationKind,
};
use temper_runtime::scheduler::sim_uuid;
use tracing::Span;

use crate::request_context::AgentContext;

use super::WasmEntityRef;

/// Derive a bounded operation identity for one integration invocation.
///
/// Request idempotency keys are intentionally unrestricted HTTP values, so
/// they cannot be copied into the envelope's ASCII-token field. Hashing keeps
/// the full source identity causal and deterministic while the integration
/// name and source kind distinguish sibling triggers from the same action.
pub(super) fn integration_operation(
    source_kind: &str,
    operation_kind: &str,
    causal_id: Option<&str>,
    scope: [&str; 5],
) -> Result<CausalOperationV1, FailureContractError> {
    let causal_id = causal_id
        .map(str::to_string)
        .unwrap_or_else(|| format!("generated:{}", sim_uuid()));
    let parent_digest = digest_parts(&[
        "temper-dispatch-v1",
        &causal_id,
        scope[0],
        scope[1],
        scope[2],
        scope[3],
    ]);
    let operation_digest = digest_parts(&[
        "temper-integration-operation-v1",
        source_kind,
        &causal_id,
        scope[0],
        scope[1],
        scope[2],
        scope[3],
        scope[4],
    ]);
    Ok(CausalOperationV1 {
        id: OperationId::new(format!("{source_kind}:{operation_digest}"))?,
        kind: OperationKind::new(operation_kind)?,
        attempt: OperationAttempt::new(1)?,
        parent_id: Some(OperationId::new(format!("dispatch:{parent_digest}"))?),
    })
}

fn digest_parts(parts: &[&str]) -> String {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update(part.as_bytes());
        digest.update([0]);
    }
    format!("{:x}", digest.finalize())
}

/// Derive a callback delivery identity without replaying the source action's
/// idempotency key against a different action on the same entity.
pub(super) fn typed_failure_callback_context(
    parent: &AgentContext,
    operation_id: &OperationId,
    callback_action: &str,
) -> AgentContext {
    let mut callback = parent.clone();
    callback.idempotency_key = Some(format!(
        "{}:failure-callback:{callback_action}",
        operation_id.as_str()
    ));
    callback.expected_entity_sequence = None;
    callback
}

/// Change callback authority for background delivery while retaining its
/// deterministic child idempotency identity.
pub(super) fn background_callback_context(
    service_name: &str,
    parent: &AgentContext,
    preserve_idempotency: bool,
) -> AgentContext {
    let mut callback = AgentContext::for_service_inheriting(service_name, parent);
    if preserve_idempotency {
        callback.idempotency_key = parent.idempotency_key.clone();
    }
    callback.expected_entity_sequence = None;
    callback
}

impl crate::state::ServerState {
    /// Record bounded control fields and safe details without diagnostic text.
    pub(super) fn record_typed_failure_observation(
        &self,
        entity_ref: WasmEntityRef<'_>,
        integration_name: &str,
        source_action: &str,
        envelope: &FailureEnvelopeV1,
    ) {
        let category = category_name(envelope.category);
        let span = Span::current();
        span.record("error.type", envelope.code.as_str());
        span.record("failure.category", category);
        span.record("failure.code", envelope.code.as_str());

        let sequence = self.next_entity_event_sequence(
            entity_ref.tenant.as_str(),
            entity_ref.entity_type,
            entity_ref.entity_id,
        );
        self.record_entity_observe_event_with_seq(
            entity_ref.tenant.as_str(),
            entity_ref.entity_type,
            entity_ref.entity_id,
            sequence,
            "typed_integration_failure",
            serde_json::json!({
                "seq": sequence,
                "integration": integration_name,
                "source_action": source_action,
                "failure": {
                    "version": envelope.version,
                    "category": envelope.category,
                    "code": envelope.code,
                    "retryability": envelope.retryability,
                    "outcome": envelope.outcome,
                    "operation": envelope.operation,
                    "provenance": envelope.provenance,
                    "diagnostic_redacted": envelope.message.is_some()
                        || envelope.diagnostic_omitted,
                    "details": envelope.details,
                    "details_omitted": envelope.details_omitted,
                },
            }),
        );
    }
}

const fn category_name(category: FailureCategory) -> &'static str {
    match category {
        FailureCategory::Transient => "transient",
        FailureCategory::Integrity => "integrity",
        FailureCategory::Authorization => "authorization",
        FailureCategory::Budget => "budget",
        FailureCategory::Ambiguous => "ambiguous",
        FailureCategory::Permanent => "permanent",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routed_callback_uses_a_child_idempotency_identity() {
        let mut parent = AgentContext {
            idempotency_key: Some("source-operation".to_string()),
            expected_entity_sequence: Some(7),
            ..AgentContext::default()
        };
        parent.agent_id = Some("caller".to_string());
        let operation = OperationId::new("source-operation").expect("valid operation id");

        let callback = typed_failure_callback_context(&parent, &operation, "Recover");

        assert_eq!(
            callback.idempotency_key.as_deref(),
            Some("source-operation:failure-callback:Recover")
        );
        assert_ne!(callback.idempotency_key, parent.idempotency_key);
        assert_eq!(callback.agent_id, parent.agent_id);
        assert_eq!(callback.expected_entity_sequence, None);
    }

    #[test]
    fn integration_identity_is_bounded_and_distinguishes_sibling_triggers() {
        let unrestricted = format!("customer key / with spaces / {}", "🦄".repeat(200));
        let scope = |integration| ["tenant-a", "Payment", "payment-1", "Charge", integration];
        let first = integration_operation(
            "wasm",
            "wasm.invoke",
            Some(&unrestricted),
            scope("first-trigger"),
        )
        .expect("unrestricted dispatch identity should derive safely");
        let repeated = integration_operation(
            "wasm",
            "wasm.invoke",
            Some(&unrestricted),
            scope("first-trigger"),
        )
        .expect("same input should derive safely");
        let sibling = integration_operation(
            "wasm",
            "wasm.invoke",
            Some(&unrestricted),
            scope("second-trigger"),
        )
        .expect("sibling trigger should derive safely");
        let other_actor = integration_operation(
            "wasm",
            "wasm.invoke",
            Some(&unrestricted),
            [
                "tenant-a",
                "Payment",
                "payment-2",
                "Charge",
                "first-trigger",
            ],
        )
        .expect("actor scope should derive safely");

        assert_eq!(first, repeated);
        assert_ne!(first.id, sibling.id);
        assert_ne!(first.id, other_actor.id);
        assert_eq!(first.parent_id, sibling.parent_id);
        assert_ne!(first.parent_id, other_actor.parent_id);
        assert!(first.id.as_str().len() <= temper_failure::MAX_OPERATION_ID_BYTES);
    }

    #[test]
    fn background_authority_keeps_the_child_delivery_identity() {
        let parent = AgentContext {
            idempotency_key: Some("wasm:child:failure-callback:Recover".to_string()),
            expected_entity_sequence: Some(9),
            ..AgentContext::default()
        };

        let callback = background_callback_context("wasm-runtime", &parent, true);

        assert_eq!(callback.idempotency_key, parent.idempotency_key);
        assert_eq!(callback.expected_entity_sequence, None);
        assert_eq!(callback.agent_type.as_deref(), Some("wasm-runtime"));

        let ordinary = background_callback_context("wasm-runtime", &parent, false);
        assert_eq!(ordinary.idempotency_key, None);
    }
}
