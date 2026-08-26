//! Shared redaction shape for typed failure observation.

/// Render the bounded control surface and safe details without diagnostic text.
pub(crate) fn redacted_failure_value(
    envelope: &temper_failure::FailureEnvelopeV1,
) -> serde_json::Value {
    serde_json::json!({
        "version": envelope.version,
        "category": envelope.category,
        "code": envelope.code,
        "retryability": envelope.retryability,
        "outcome": envelope.outcome,
        "operation": envelope.operation,
        "provenance": envelope.provenance,
        "diagnostic_redacted": envelope.message.is_some() || envelope.diagnostic_omitted,
        "details": envelope.details,
        "details_omitted": envelope.details_omitted,
    })
}

/// Render a guest-declared failure without its diagnostic or detail values.
pub(crate) fn redacted_guest_failure_value(
    envelope: &temper_failure::FailureEnvelopeV1,
) -> serde_json::Value {
    serde_json::json!({
        "version": envelope.version,
        "category": envelope.category,
        "code": envelope.code,
        "retryability": envelope.retryability,
        "outcome": envelope.outcome,
        "operation": envelope.operation,
        "provenance": envelope.provenance,
        "diagnostic_redacted": envelope.message.is_some() || envelope.diagnostic_omitted,
        "details": {},
        "details_redacted": !envelope.details.values().is_empty(),
        "details_omitted": envelope.details_omitted,
    })
}

#[cfg(test)]
mod tests {
    use temper_failure::{
        CausalOperationV1, FailureCategory, FailureEnvelopeV1, FailureOutcome, FailureProvenanceV1,
        FailureRetryability, FailureSource, OperationAttempt, OperationId, OperationKind,
        ProvenanceToken, StableFailureCode,
    };

    #[test]
    fn observation_never_contains_diagnostic_text() {
        let envelope = FailureEnvelopeV1::new(
            FailureCategory::Permanent,
            StableFailureCode::new("TerminalFailure").expect("valid code"),
            FailureRetryability::Never,
            FailureOutcome::NotApplied,
            CausalOperationV1 {
                id: OperationId::new("delivery-1").expect("valid operation id"),
                kind: OperationKind::new("reaction.deliver").expect("valid operation kind"),
                attempt: OperationAttempt::new(1).expect("valid attempt"),
                parent_id: None,
            },
            FailureProvenanceV1 {
                source: FailureSource::Reaction,
                component: ProvenanceToken::new("temper-server.delivery").expect("valid component"),
                source_code: None,
            },
        )
        .expect("valid envelope")
        .with_diagnostic("private diagnostic");

        let value = super::redacted_failure_value(&envelope);
        assert!(value.get("message").is_none());
        assert_eq!(value["diagnostic_redacted"], true);
        assert!(!value.to_string().contains("private diagnostic"));
    }

    #[test]
    fn guest_observation_redacts_diagnostic_and_details() {
        let mut envelope = FailureEnvelopeV1::new(
            FailureCategory::Permanent,
            StableFailureCode::new("ProviderRejected").expect("valid code"),
            FailureRetryability::Never,
            FailureOutcome::NotApplied,
            CausalOperationV1 {
                id: OperationId::new("wasm-1").expect("valid operation id"),
                kind: OperationKind::new("wasm.invoke").expect("valid operation kind"),
                attempt: OperationAttempt::new(1).expect("valid attempt"),
                parent_id: None,
            },
            FailureProvenanceV1 {
                source: FailureSource::Wasm,
                component: ProvenanceToken::new("wasm-guest").expect("valid component"),
                source_code: None,
            },
        )
        .expect("valid envelope")
        .with_diagnostic("token=private");
        envelope.insert_detail_or_omit(
            temper_failure::DetailKey::new("provider_token").expect("valid detail key"),
            temper_failure::FailureDetailValue::String(
                temper_failure::BoundedDetailString::new("private-value").expect("bounded detail"),
            ),
        );

        let value = super::redacted_guest_failure_value(&envelope);
        assert_eq!(value["details"], serde_json::json!({}));
        assert_eq!(value["details_redacted"], true);
        assert!(!value.to_string().contains("token=private"));
        assert!(!value.to_string().contains("provider_token"));
        assert!(!value.to_string().contains("private-value"));
    }
}
