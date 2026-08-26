//! Guest-declared application facts for a typed WASM terminal failure.

use serde::{Deserialize, Deserializer, Serialize, de};

use crate::bounds::FAILURE_ENVELOPE_VERSION_V1;
use crate::envelope::validate_failure_semantics;
use crate::{
    BoundedDiagnostic, BoundedFailureDetails, DetailKey, FailureCategory, FailureContractError,
    FailureDetailValue, FailureOutcome, FailureRetryability, StableFailureCode,
};

/// Bounded v1 application facts a WASM guest may declare for a terminal failure.
///
/// Causal operation identity and provenance are intentionally absent. The kernel
/// owns those fields and uses this declaration to construct the canonical
/// [`crate::FailureEnvelopeV1`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestFailureDeclarationV1 {
    /// Wire-contract version; always one for this type.
    pub version: u16,
    /// Closed routing category claimed by the application guest.
    pub category: FailureCategory,
    /// Stable application-specific failure code.
    pub code: StableFailureCode,
    /// Guidance for a later, separately governed operation.
    pub retryability: FailureRetryability,
    /// Whether the guest knows that its causal external operation applied.
    pub outcome: FailureOutcome,
    /// Optional bounded diagnostic, never used for control flow.
    pub diagnostic: Option<BoundedDiagnostic>,
    /// Optional bounded scalar application details.
    pub details: BoundedFailureDetails,
}

impl GuestFailureDeclarationV1 {
    /// Construct a validated declaration without diagnostic text or details.
    pub fn new(
        category: FailureCategory,
        code: StableFailureCode,
        retryability: FailureRetryability,
        outcome: FailureOutcome,
    ) -> Result<Self, FailureContractError> {
        validate_failure_semantics(category, retryability, outcome)?;
        Ok(Self {
            version: FAILURE_ENVELOPE_VERSION_V1,
            category,
            code,
            retryability,
            outcome,
            diagnostic: None,
            details: BoundedFailureDetails::default(),
        })
    }

    /// Attach a diagnostic when it satisfies the canonical v1 byte budget.
    pub fn with_diagnostic(
        mut self,
        diagnostic: impl Into<String>,
    ) -> Result<Self, FailureContractError> {
        self.diagnostic = Some(BoundedDiagnostic::new(diagnostic)?);
        Ok(self)
    }

    /// Insert one bounded scalar detail without truncation or omission.
    pub fn try_insert_detail(
        &mut self,
        key: DetailKey,
        value: FailureDetailValue,
    ) -> Result<(), FailureContractError> {
        self.details.try_insert(key, value)
    }
}

impl Serialize for GuestFailureDeclarationV1 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        validate_declaration(self).map_err(serde::ser::Error::custom)?;

        #[derive(Serialize)]
        struct WireDeclaration<'a> {
            version: u16,
            category: FailureCategory,
            code: &'a StableFailureCode,
            retryability: FailureRetryability,
            outcome: FailureOutcome,
            #[serde(skip_serializing_if = "Option::is_none")]
            diagnostic: &'a Option<BoundedDiagnostic>,
            details: &'a BoundedFailureDetails,
        }

        WireDeclaration {
            version: self.version,
            category: self.category,
            code: &self.code,
            retryability: self.retryability,
            outcome: self.outcome,
            diagnostic: &self.diagnostic,
            details: &self.details,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for GuestFailureDeclarationV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireDeclaration {
            version: u16,
            category: FailureCategory,
            code: StableFailureCode,
            retryability: FailureRetryability,
            outcome: FailureOutcome,
            #[serde(default)]
            diagnostic: Option<BoundedDiagnostic>,
            #[serde(default)]
            details: BoundedFailureDetails,
        }

        let wire = WireDeclaration::deserialize(deserializer)?;
        let declaration = Self {
            version: wire.version,
            category: wire.category,
            code: wire.code,
            retryability: wire.retryability,
            outcome: wire.outcome,
            diagnostic: wire.diagnostic,
            details: wire.details,
        };
        validate_declaration(&declaration).map_err(de::Error::custom)?;
        Ok(declaration)
    }
}

fn validate_declaration(
    declaration: &GuestFailureDeclarationV1,
) -> Result<(), FailureContractError> {
    if declaration.version != FAILURE_ENVELOPE_VERSION_V1 {
        return Err(FailureContractError::UnsupportedVersion {
            expected: FAILURE_ENVELOPE_VERSION_V1,
            actual: declaration.version,
        });
    }
    validate_failure_semantics(
        declaration.category,
        declaration.retryability,
        declaration.outcome,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_v1_encoding_contains_only_guest_owned_fields() {
        let mut declaration = GuestFailureDeclarationV1::new(
            FailureCategory::Transient,
            StableFailureCode::new("ProviderUnavailable").expect("valid code"),
            FailureRetryability::WithBackoff,
            FailureOutcome::NotApplied,
        )
        .expect("valid declaration")
        .with_diagnostic("provider did not accept the request")
        .expect("bounded diagnostic");
        declaration
            .try_insert_detail(
                DetailKey::new("status").expect("valid key"),
                FailureDetailValue::Unsigned(503),
            )
            .expect("bounded detail");

        assert_eq!(
            serde_json::to_string(&declaration).expect("encode declaration"),
            r#"{"version":1,"category":"transient","code":"ProviderUnavailable","retryability":"with_backoff","outcome":"not_applied","diagnostic":"provider did not accept the request","details":{"status":{"kind":"unsigned","value":503}}}"#
        );
    }

    #[test]
    fn unknown_and_kernel_owned_fields_fail_closed() {
        let base = serde_json::json!({
            "version": 1,
            "category": "permanent",
            "code": "ProviderRejected",
            "retryability": "never",
            "outcome": "not_applied"
        });
        for field in [
            "operation",
            "provenance",
            "message",
            "diagnostic_omitted",
            "details_omitted",
            "unknown",
        ] {
            let mut injected = base.clone();
            injected[field] = serde_json::json!({});
            assert!(
                serde_json::from_value::<GuestFailureDeclarationV1>(injected).is_err(),
                "accepted forbidden field {field}"
            );
        }
    }

    #[test]
    fn future_versions_and_oversized_diagnostics_fail_closed() {
        let future = serde_json::json!({
            "version": 2,
            "category": "permanent",
            "code": "ProviderRejected",
            "retryability": "never",
            "outcome": "not_applied"
        });
        assert!(serde_json::from_value::<GuestFailureDeclarationV1>(future).is_err());

        let oversized = serde_json::json!({
            "version": 1,
            "category": "permanent",
            "code": "ProviderRejected",
            "retryability": "never",
            "outcome": "not_applied",
            "diagnostic": "x".repeat(crate::MAX_DIAGNOSTIC_BYTES + 1)
        });
        assert!(serde_json::from_value::<GuestFailureDeclarationV1>(oversized).is_err());
    }

    #[test]
    fn serialization_revalidates_public_fields() {
        let mut declaration = GuestFailureDeclarationV1::new(
            FailureCategory::Permanent,
            StableFailureCode::new("ProviderRejected").expect("valid code"),
            FailureRetryability::Never,
            FailureOutcome::NotApplied,
        )
        .expect("valid declaration");
        declaration.version = 2;
        assert!(serde_json::to_value(&declaration).is_err());
    }

    #[test]
    fn every_valid_category_retryability_outcome_combination_is_accepted() {
        let categories = [
            FailureCategory::Transient,
            FailureCategory::Integrity,
            FailureCategory::Authorization,
            FailureCategory::Budget,
            FailureCategory::Ambiguous,
            FailureCategory::Permanent,
        ];
        let retryabilities = [
            FailureRetryability::Never,
            FailureRetryability::AfterRefresh,
            FailureRetryability::WithBackoff,
            FailureRetryability::AfterAuthorization,
            FailureRetryability::Reconcile,
        ];
        let outcomes = [
            FailureOutcome::NotApplied,
            FailureOutcome::Applied,
            FailureOutcome::Unknown,
        ];

        for category in categories {
            for retryability in retryabilities {
                for outcome in outcomes {
                    let expected_valid = (outcome == FailureOutcome::Unknown)
                        == (retryability == FailureRetryability::Reconcile)
                        && (category != FailureCategory::Ambiguous
                            || outcome == FailureOutcome::Unknown);
                    let actual = GuestFailureDeclarationV1::new(
                        category,
                        StableFailureCode::new("GuestFailure").expect("valid code"),
                        retryability,
                        outcome,
                    );
                    assert_eq!(
                        actual.is_ok(),
                        expected_valid,
                        "category={category:?} retryability={retryability:?} outcome={outcome:?}"
                    );
                }
            }
        }
    }
}
