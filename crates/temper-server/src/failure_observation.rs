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
}
