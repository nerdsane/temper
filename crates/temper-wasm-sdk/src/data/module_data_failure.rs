//! Structured module-data error adaptation.

use super::{ModuleDataError, ModuleDataErrorKind, Retryability};
use temper_failure::{
    BoundedDetailString, CausalOperationV1, DetailKey, FailureCategory, FailureContractError,
    FailureDetailValue, FailureEnvelopeV1, FailureOutcome, FailureProvenanceV1,
    FailureRetryability, FailureSource, ProvenanceToken, StableFailureCode,
};

/// Adapt a structured module-data error without inspecting its diagnostic text.
pub fn adapt_module_data_error(
    error: &ModuleDataError,
    operation: CausalOperationV1,
    outcome: FailureOutcome,
) -> Result<FailureEnvelopeV1, FailureContractError> {
    let category = if outcome == FailureOutcome::Unknown {
        FailureCategory::Ambiguous
    } else {
        category_for(error.kind)
    };
    let retryability = if outcome == FailureOutcome::Unknown {
        FailureRetryability::Reconcile
    } else {
        retryability_for(error.retryability)
    };
    let provenance = FailureProvenanceV1 {
        source: FailureSource::ModuleData,
        component: ProvenanceToken::new("module-data")?,
        source_code: Some(ProvenanceToken::new(source_code(error.kind))?),
    };
    let mut envelope = FailureEnvelopeV1::new(
        category,
        StableFailureCode::new(error.code.clone())?,
        retryability,
        outcome,
        operation,
        provenance,
    )?
    .with_diagnostic(error.message.clone());

    if let Some(decision_id) = &error.decision_id {
        match (
            DetailKey::new("decision_id"),
            BoundedDetailString::new(decision_id.clone()),
        ) {
            (Ok(key), Ok(value)) => {
                envelope.insert_detail_or_omit(key, FailureDetailValue::String(value));
            }
            _ => envelope.details_omitted = true,
        }
    }
    if error.details.is_some() {
        envelope.details_omitted = true;
    }
    Ok(envelope)
}

fn category_for(kind: ModuleDataErrorKind) -> FailureCategory {
    match kind {
        ModuleDataErrorKind::InvalidRequest
        | ModuleDataErrorKind::SchemaMismatch
        | ModuleDataErrorKind::NotFound
        | ModuleDataErrorKind::AlreadyExists
        | ModuleDataErrorKind::GuardRejected
        | ModuleDataErrorKind::RelationViolation
        | ModuleDataErrorKind::VerificationFailed
        | ModuleDataErrorKind::Conflict => FailureCategory::Integrity,
        ModuleDataErrorKind::AuthorizationDenied => FailureCategory::Authorization,
        ModuleDataErrorKind::ConsistencyUnavailable | ModuleDataErrorKind::TransientUnavailable => {
            FailureCategory::Transient
        }
        ModuleDataErrorKind::BudgetExceeded => FailureCategory::Budget,
        ModuleDataErrorKind::Internal => FailureCategory::Permanent,
    }
}

fn retryability_for(retryability: Retryability) -> FailureRetryability {
    match retryability {
        Retryability::Never => FailureRetryability::Never,
        Retryability::AfterRefresh => FailureRetryability::AfterRefresh,
        Retryability::WithBackoff => FailureRetryability::WithBackoff,
    }
}

fn source_code(kind: ModuleDataErrorKind) -> &'static str {
    match kind {
        ModuleDataErrorKind::InvalidRequest => "InvalidRequest",
        ModuleDataErrorKind::SchemaMismatch => "SchemaMismatch",
        ModuleDataErrorKind::NotFound => "NotFound",
        ModuleDataErrorKind::AlreadyExists => "AlreadyExists",
        ModuleDataErrorKind::AuthorizationDenied => "AuthorizationDenied",
        ModuleDataErrorKind::GuardRejected => "GuardRejected",
        ModuleDataErrorKind::RelationViolation => "RelationViolation",
        ModuleDataErrorKind::VerificationFailed => "VerificationFailed",
        ModuleDataErrorKind::Conflict => "Conflict",
        ModuleDataErrorKind::ConsistencyUnavailable => "ConsistencyUnavailable",
        ModuleDataErrorKind::BudgetExceeded => "BudgetExceeded",
        ModuleDataErrorKind::TransientUnavailable => "TransientUnavailable",
        ModuleDataErrorKind::Internal => "Internal",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use temper_failure::{OperationAttempt, OperationId, OperationKind};

    fn operation() -> CausalOperationV1 {
        CausalOperationV1 {
            id: OperationId::new("module-call:42").expect("valid id"),
            kind: OperationKind::new("module_data.action").expect("valid kind"),
            attempt: OperationAttempt::new(1).expect("valid attempt"),
            parent_id: None,
        }
    }

    #[test]
    fn every_module_data_kind_has_a_typed_category() {
        let cases = [
            (
                ModuleDataErrorKind::InvalidRequest,
                FailureCategory::Integrity,
            ),
            (
                ModuleDataErrorKind::SchemaMismatch,
                FailureCategory::Integrity,
            ),
            (ModuleDataErrorKind::NotFound, FailureCategory::Integrity),
            (
                ModuleDataErrorKind::AlreadyExists,
                FailureCategory::Integrity,
            ),
            (
                ModuleDataErrorKind::AuthorizationDenied,
                FailureCategory::Authorization,
            ),
            (
                ModuleDataErrorKind::GuardRejected,
                FailureCategory::Integrity,
            ),
            (
                ModuleDataErrorKind::RelationViolation,
                FailureCategory::Integrity,
            ),
            (
                ModuleDataErrorKind::VerificationFailed,
                FailureCategory::Integrity,
            ),
            (ModuleDataErrorKind::Conflict, FailureCategory::Integrity),
            (
                ModuleDataErrorKind::ConsistencyUnavailable,
                FailureCategory::Transient,
            ),
            (ModuleDataErrorKind::BudgetExceeded, FailureCategory::Budget),
            (
                ModuleDataErrorKind::TransientUnavailable,
                FailureCategory::Transient,
            ),
            (ModuleDataErrorKind::Internal, FailureCategory::Permanent),
        ];
        for (kind, expected) in cases {
            let error = ModuleDataError::new(kind, "StableCode", "diagnostic", Retryability::Never);
            let envelope = adapt_module_data_error(&error, operation(), FailureOutcome::NotApplied)
                .expect("valid adaptation");
            assert_eq!(envelope.category, expected, "kind {kind:?}");
        }
    }

    #[test]
    fn unknown_outcome_overrides_retry_with_reconciliation() {
        let error = ModuleDataError::new(
            ModuleDataErrorKind::TransientUnavailable,
            "AcknowledgementLost",
            "provider acknowledgement was not observed",
            Retryability::WithBackoff,
        );
        let envelope = adapt_module_data_error(&error, operation(), FailureOutcome::Unknown)
            .expect("valid ambiguous adaptation");
        assert_eq!(envelope.category, FailureCategory::Ambiguous);
        assert_eq!(envelope.retryability, FailureRetryability::Reconcile);
        assert_eq!(envelope.outcome, FailureOutcome::Unknown);
    }

    #[test]
    fn decision_provenance_is_preserved_but_unbounded_metadata_is_omitted() {
        let mut error = ModuleDataError::new(
            ModuleDataErrorKind::AuthorizationDenied,
            "AuthorizationDenied",
            "approval required",
            Retryability::Never,
        );
        error.decision_id = Some("PD-123".into());
        error.details = Some(Box::new(serde_json::Map::from_iter([(
            "provider_payload".into(),
            serde_json::json!({"nested": "unsafe"}),
        )])));
        let envelope = adapt_module_data_error(&error, operation(), FailureOutcome::NotApplied)
            .expect("valid authorization adaptation");
        assert_eq!(envelope.category, FailureCategory::Authorization);
        assert!(envelope.details_omitted);
        assert!(matches!(
            envelope.details.values().get(
                &DetailKey::new("decision_id").expect("static detail key")
            ),
            Some(FailureDetailValue::String(value)) if value.as_str() == "PD-123"
        ));
        let encoded = serde_json::to_vec(&envelope).expect("adapter output must serialize");
        let decoded: FailureEnvelopeV1 =
            serde_json::from_slice(&encoded).expect("adapter output must round trip");
        assert_eq!(decoded, envelope);
    }

    #[test]
    fn invalid_required_code_fails_closed_and_long_diagnostic_is_omitted() {
        let invalid = ModuleDataError::new(
            ModuleDataErrorKind::Internal,
            "not a stable code",
            "diagnostic",
            Retryability::Never,
        );
        assert!(
            adapt_module_data_error(&invalid, operation(), FailureOutcome::NotApplied).is_err()
        );

        let long = ModuleDataError::new(
            ModuleDataErrorKind::Internal,
            "InternalFailure",
            "x".repeat(temper_failure::MAX_DIAGNOSTIC_BYTES + 1),
            Retryability::Never,
        );
        let envelope = adapt_module_data_error(&long, operation(), FailureOutcome::NotApplied)
            .expect("required fields remain valid");
        assert!(envelope.message.is_none());
        assert!(envelope.diagnostic_omitted);
    }
}
