use std::collections::BTreeMap;

use super::*;

fn operation() -> CausalOperationV1 {
    CausalOperationV1 {
        id: OperationId::new("payment:charge:42").expect("valid operation id"),
        kind: OperationKind::new("payment.charge").expect("valid operation kind"),
        attempt: OperationAttempt::new(2).expect("valid attempt"),
        parent_id: Some(OperationId::new("checkout:42").expect("valid parent id")),
    }
}

fn provenance() -> FailureProvenanceV1 {
    FailureProvenanceV1 {
        source: FailureSource::Authorization,
        component: ProvenanceToken::new("cedar-gate").expect("valid component"),
        source_code: Some(ProvenanceToken::new("AuthorizationDenied").expect("valid source code")),
    }
}

#[test]
fn exact_v1_encoding_is_stable() {
    let mut envelope = FailureEnvelopeV1::new(
        FailureCategory::Authorization,
        StableFailureCode::new("AuthorizationDenied").expect("valid failure code"),
        FailureRetryability::AfterAuthorization,
        FailureOutcome::NotApplied,
        operation(),
        provenance(),
    )
    .expect("valid envelope")
    .with_diagnostic("approval required");
    envelope.insert_detail_or_omit(
        DetailKey::new("decision_id").expect("valid detail key"),
        FailureDetailValue::String(BoundedDetailString::new("PD-123").expect("valid detail value")),
    );

    assert_eq!(
        serde_json::to_string(&envelope).expect("encode envelope"),
        r#"{"version":1,"category":"authorization","code":"AuthorizationDenied","retryability":"after_authorization","outcome":"not_applied","operation":{"id":"payment:charge:42","kind":"payment.charge","attempt":2,"parent_id":"checkout:42"},"provenance":{"source":"authorization","component":"cedar-gate","source_code":"AuthorizationDenied"},"message":"approval required","diagnostic_omitted":false,"details":{"decision_id":{"kind":"string","value":"PD-123"}},"details_omitted":false}"#
    );
}

#[test]
fn unknown_fields_and_versions_fail_closed() {
    let encoded = serde_json::to_value(
        FailureEnvelopeV1::new(
            FailureCategory::Permanent,
            StableFailureCode::new("TerminalFailure").expect("valid code"),
            FailureRetryability::Never,
            FailureOutcome::NotApplied,
            operation(),
            provenance(),
        )
        .expect("valid envelope"),
    )
    .expect("encode envelope");

    let mut unknown = encoded.clone();
    unknown["extra"] = serde_json::json!(true);
    assert!(serde_json::from_value::<FailureEnvelopeV1>(unknown).is_err());

    let mut future = encoded;
    future["version"] = serde_json::json!(2);
    assert!(serde_json::from_value::<FailureEnvelopeV1>(future).is_err());
}

#[test]
fn contradictory_omission_states_fail_closed() {
    let encoded = serde_json::to_value(
        FailureEnvelopeV1::new(
            FailureCategory::Permanent,
            StableFailureCode::new("TerminalFailure").expect("valid code"),
            FailureRetryability::Never,
            FailureOutcome::NotApplied,
            operation(),
            provenance(),
        )
        .expect("valid envelope"),
    )
    .expect("encode envelope");

    let mut contradictory_message = encoded.clone();
    contradictory_message["message"] = serde_json::json!("present");
    contradictory_message["diagnostic_omitted"] = serde_json::json!(true);
    assert!(serde_json::from_value::<FailureEnvelopeV1>(contradictory_message).is_err());

    let mut partial_details = encoded;
    partial_details["details"] = serde_json::json!({"safe":{"kind":"bool","value":true}});
    partial_details["details_omitted"] = serde_json::json!(true);
    let decoded = serde_json::from_value::<FailureEnvelopeV1>(partial_details)
        .expect("safe retained details may coexist with an upstream omission marker");
    assert!(decoded.details_omitted);
}

#[test]
fn serialization_revalidates_public_fields() {
    let mut envelope = FailureEnvelopeV1::new(
        FailureCategory::Permanent,
        StableFailureCode::new("TerminalFailure").expect("valid code"),
        FailureRetryability::Never,
        FailureOutcome::NotApplied,
        operation(),
        provenance(),
    )
    .expect("valid envelope");
    envelope.version = 2;
    assert!(serde_json::to_value(&envelope).is_err());

    envelope.version = FAILURE_ENVELOPE_VERSION_V1;
    envelope.outcome = FailureOutcome::Unknown;
    assert!(serde_json::to_value(&envelope).is_err());
}

#[test]
fn every_variable_field_rejects_its_budget_plus_one() {
    assert!(StableFailureCode::new("x".repeat(MAX_FAILURE_CODE_BYTES + 1)).is_err());
    assert!(OperationId::new("x".repeat(MAX_OPERATION_ID_BYTES + 1)).is_err());
    assert!(OperationKind::new("x".repeat(MAX_OPERATION_KIND_BYTES + 1)).is_err());
    assert!(ProvenanceToken::new("x".repeat(MAX_PROVENANCE_TOKEN_BYTES + 1)).is_err());
    assert!(DetailKey::new("x".repeat(MAX_DETAIL_KEY_BYTES + 1)).is_err());
    assert!(BoundedDiagnostic::new("x".repeat(MAX_DIAGNOSTIC_BYTES + 1)).is_err());
    assert!(BoundedDetailString::new("x".repeat(MAX_DETAIL_STRING_BYTES + 1)).is_err());
    assert!(OperationAttempt::new(MAX_OPERATION_ATTEMPT + 1).is_err());
}

#[test]
fn token_allowlist_rejects_spaces_unicode_and_slashes() {
    for invalid in ["has space", "unicode-λ", "path/name"] {
        assert!(
            StableFailureCode::new(invalid).is_err(),
            "accepted {invalid}"
        );
    }
}

#[test]
fn details_are_bounded_by_count_and_total_encoding() {
    let mut too_many = BTreeMap::new();
    for index in 0..=MAX_DETAIL_ENTRIES {
        too_many.insert(
            DetailKey::new(format!("key-{index}")).expect("valid key"),
            FailureDetailValue::Bool(true),
        );
    }
    assert!(matches!(
        BoundedFailureDetails::new(too_many),
        Err(FailureContractError::TooManyDetails { .. })
    ));

    let mut too_large = BTreeMap::new();
    for index in 0..MAX_DETAIL_ENTRIES {
        too_large.insert(
            DetailKey::new(format!("key-{index}")).expect("valid key"),
            FailureDetailValue::String(
                BoundedDetailString::new("x".repeat(MAX_DETAIL_STRING_BYTES))
                    .expect("individually bounded value"),
            ),
        );
    }
    assert!(matches!(
        BoundedFailureDetails::new(too_large),
        Err(FailureContractError::DetailsTooLarge { .. })
    ));
}

#[test]
fn oversized_optional_values_are_omitted_as_complete_fields() {
    let mut envelope = FailureEnvelopeV1::new(
        FailureCategory::Permanent,
        StableFailureCode::new("ProviderFailure").expect("valid code"),
        FailureRetryability::Never,
        FailureOutcome::NotApplied,
        operation(),
        provenance(),
    )
    .expect("valid envelope")
    .with_diagnostic("x".repeat(MAX_DIAGNOSTIC_BYTES + 1));
    assert!(envelope.message.is_none());
    assert!(envelope.diagnostic_omitted);

    for index in 0..=MAX_DETAIL_ENTRIES {
        envelope.insert_detail_or_omit(
            DetailKey::new(format!("key-{index}")).expect("valid key"),
            FailureDetailValue::Bool(true),
        );
    }
    assert!(envelope.details.values().is_empty());
    assert!(envelope.details_omitted);
}

#[test]
fn omission_marker_allows_a_safe_retained_subset() {
    let mut envelope = FailureEnvelopeV1::new(
        FailureCategory::Permanent,
        StableFailureCode::new("ProviderFailure").expect("valid code"),
        FailureRetryability::Never,
        FailureOutcome::NotApplied,
        operation(),
        provenance(),
    )
    .expect("valid envelope");
    envelope.details_omitted = true;
    envelope.insert_detail_or_omit(
        DetailKey::new("late_detail").expect("valid key"),
        FailureDetailValue::Bool(true),
    );
    assert_eq!(envelope.details.values().len(), 1);
}

#[test]
fn a_later_valid_diagnostic_clears_prior_omission() {
    let envelope = FailureEnvelopeV1::new(
        FailureCategory::Permanent,
        StableFailureCode::new("ProviderFailure").expect("valid code"),
        FailureRetryability::Never,
        FailureOutcome::NotApplied,
        operation(),
        provenance(),
    )
    .expect("valid envelope")
    .with_diagnostic("x".repeat(MAX_DIAGNOSTIC_BYTES + 1))
    .with_diagnostic("bounded diagnostic");
    assert_eq!(
        envelope.message.as_ref().map(BoundedDiagnostic::as_str),
        Some("bounded diagnostic")
    );
    assert!(!envelope.diagnostic_omitted);
    assert!(serde_json::to_value(envelope).is_ok());
}

#[test]
fn ambiguity_and_reconciliation_semantics_are_validated() {
    let ambiguous = FailureEnvelopeV1::new(
        FailureCategory::Ambiguous,
        StableFailureCode::new("AcknowledgementLost").expect("valid code"),
        FailureRetryability::Reconcile,
        FailureOutcome::Unknown,
        operation(),
        provenance(),
    );
    assert!(ambiguous.is_ok());

    assert!(matches!(
        FailureEnvelopeV1::new(
            FailureCategory::Ambiguous,
            StableFailureCode::new("ImpossibleKnownOutcome").expect("valid code"),
            FailureRetryability::Never,
            FailureOutcome::NotApplied,
            operation(),
            provenance(),
        ),
        Err(FailureContractError::AmbiguousCategoryWithKnownOutcome)
    ));

    assert!(matches!(
        FailureEnvelopeV1::new(
            FailureCategory::Transient,
            StableFailureCode::new("UnsafeUnknownRetry").expect("valid code"),
            FailureRetryability::WithBackoff,
            FailureOutcome::Unknown,
            operation(),
            provenance(),
        ),
        Err(FailureContractError::InvalidReconciliationGuidance)
    ));
}
