//! Typed application-failure adaptation and route selection for WASM triggers.

use temper_failure::{
    BoundedDetailString, DetailKey, FailureCategory, FailureContractError, FailureDetailValue,
    FailureEnvelopeV1, FailureOutcome, FailureProvenanceV1, FailureRetryability, FailureSource,
    ProvenanceToken, StableFailureCode,
};
use temper_spec::automaton::Integration;
use temper_wasm::WasmError;

/// Structured source of one terminal WASM-trigger failure.
pub(super) enum WasmFailure {
    /// A Cedar or capability denial observed by the authorization gate.
    Authorization(String),
    /// A typed engine failure.
    Engine(WasmError),
    /// A pre-invocation module resolution, artifact, or cache failure.
    Setup(String),
    /// An unsuccessful legacy guest result with no typed source variant.
    Legacy(String),
}

impl WasmFailure {
    /// Return diagnostic text for legacy persistence and human debugging only.
    pub(super) fn diagnostic(&self) -> String {
        match self {
            Self::Authorization(diagnostic)
            | Self::Setup(diagnostic)
            | Self::Legacy(diagnostic) => diagnostic.clone(),
            Self::Engine(error) => error.to_string(),
        }
    }

    /// Whether this failure came from the structured authorization tracker.
    pub(super) const fn is_authorization(&self) -> bool {
        matches!(self, Self::Authorization(_))
    }

    /// Adapt the source without inspecting its diagnostic text.
    pub(super) fn into_envelope(
        self,
        causal_id: Option<&str>,
        scope: [&str; 5],
        decision_id: Option<&str>,
    ) -> Result<FailureEnvelopeV1, FailureContractError> {
        let operation = super::super::typed_failure::integration_operation(
            "wasm",
            "wasm.invoke",
            causal_id,
            scope,
        )?;
        match self {
            Self::Engine(error) => temper_wasm::adapt_wasm_error(&error, operation),
            Self::Setup(diagnostic) => {
                let provenance = FailureProvenanceV1 {
                    source: FailureSource::Wasm,
                    component: ProvenanceToken::new("wasm-module-cache")?,
                    source_code: Some(ProvenanceToken::new("ModulePreparation")?),
                };
                Ok(FailureEnvelopeV1::new(
                    FailureCategory::Integrity,
                    StableFailureCode::new("ModulePreparationFailed")?,
                    FailureRetryability::AfterRefresh,
                    FailureOutcome::NotApplied,
                    operation,
                    provenance,
                )?
                .with_diagnostic(diagnostic))
            }
            Self::Authorization(diagnostic) => {
                let provenance = FailureProvenanceV1 {
                    source: FailureSource::Authorization,
                    component: ProvenanceToken::new("wasm-http-gate")?,
                    source_code: Some(ProvenanceToken::new("HttpCallDenied")?),
                };
                let mut envelope = FailureEnvelopeV1::new(
                    FailureCategory::Authorization,
                    StableFailureCode::new("AuthorizationDenied")?,
                    FailureRetryability::AfterAuthorization,
                    FailureOutcome::NotApplied,
                    operation,
                    provenance,
                )?
                .with_diagnostic(diagnostic);
                insert_decision_id(&mut envelope, decision_id);
                Ok(envelope)
            }
            Self::Legacy(diagnostic) => {
                let provenance = FailureProvenanceV1 {
                    source: FailureSource::Legacy,
                    component: ProvenanceToken::new("wasm-result")?,
                    source_code: Some(ProvenanceToken::new("LegacyGuestFailure")?),
                };
                Ok(FailureEnvelopeV1::new(
                    FailureCategory::Permanent,
                    StableFailureCode::new("LegacyFreeFormFailure")?,
                    FailureRetryability::Reconcile,
                    FailureOutcome::Unknown,
                    operation,
                    provenance,
                )?
                .with_diagnostic(diagnostic))
            }
        }
    }
}

fn insert_decision_id(envelope: &mut FailureEnvelopeV1, decision_id: Option<&str>) {
    let Some(decision_id) = decision_id else {
        return;
    };
    match (
        DetailKey::new("decision_id"),
        BoundedDetailString::new(decision_id),
    ) {
        (Ok(key), Ok(value)) => {
            envelope.insert_detail_or_omit(key, FailureDetailValue::String(value));
        }
        _ => envelope.details_omitted = true,
    }
}

/// Resolve exactly one verified callback for the envelope category.
pub(in crate::state::dispatch) fn failure_callback(
    integration: &Integration,
    category: FailureCategory,
) -> Result<&str, String> {
    integration
        .failure_routes
        .iter()
        .find(|route| route.category == category)
        .map(|route| route.callback_action.as_str())
        .ok_or_else(|| {
            format!(
                "UndeclaredFailureCategory: trigger '{}' has no '{}' failure route",
                integration.name,
                category_name(category)
            )
        })
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
    use temper_spec::automaton::ResolvedFailureRoute;

    fn integration(routes: Vec<ResolvedFailureRoute>) -> Integration {
        Integration {
            name: "charge-card".into(),
            trigger: "Charge".into(),
            integration_type: "wasm".into(),
            module: Some("payments".into()),
            on_success: None,
            on_failure: None,
            failure_routes: routes,
            llm: false,
            config: std::collections::BTreeMap::new(),
        }
    }

    fn scope(integration: &'static str) -> [&'static str; 5] {
        ["default", "Payment", "payment-1", "Charge", integration]
    }

    #[test]
    fn category_selection_uses_verified_metadata_only() {
        let integration = integration(vec![ResolvedFailureRoute {
            source_action: "Charge".into(),
            trigger_name: "charge-card".into(),
            category: FailureCategory::Authorization,
            callback_action: "AwaitApproval".into(),
        }]);
        assert_eq!(
            failure_callback(&integration, FailureCategory::Authorization),
            Ok("AwaitApproval")
        );
        let error = failure_callback(&integration, FailureCategory::Permanent)
            .expect_err("undeclared categories fail closed");
        assert!(error.starts_with("UndeclaredFailureCategory:"));
    }

    #[test]
    fn diagnostic_text_never_changes_legacy_or_authorization_classification() {
        let first = WasmFailure::Legacy("authorization denied".into())
            .into_envelope(Some("dispatch:legacy:1"), scope("first"), None)
            .expect("valid envelope");
        let second = WasmFailure::Legacy("temporary network failure".into())
            .into_envelope(Some("dispatch:legacy:2"), scope("second"), None)
            .expect("valid envelope");
        assert_eq!(first.category, FailureCategory::Permanent);
        assert_eq!(first.category, second.category);

        let authorization = WasmFailure::Authorization("permanent failure".into())
            .into_envelope(
                Some("dispatch:authorization:1"),
                scope("authorization"),
                Some("PD-123"),
            )
            .expect("valid envelope");
        assert_eq!(authorization.category, FailureCategory::Authorization);
        assert_eq!(
            authorization.retryability,
            FailureRetryability::AfterAuthorization
        );

        let setup = WasmFailure::Setup("authorization denied".into())
            .into_envelope(Some("dispatch:setup:1"), scope("setup"), None)
            .expect("valid envelope");
        assert_eq!(setup.category, FailureCategory::Integrity);
        assert_eq!(setup.outcome, FailureOutcome::NotApplied);
        assert_eq!(setup.retryability, FailureRetryability::AfterRefresh);
    }

    #[test]
    fn seeded_failure_envelopes_are_bit_exact_and_do_not_extend_attempt_budgets() {
        fn encoded(seed: u64) -> Vec<u8> {
            let (_guard, _clock, _ids) =
                temper_runtime::scheduler::install_deterministic_context(seed);
            let envelope = WasmFailure::Engine(WasmError::FuelExhausted)
                .into_envelope(None, scope("seeded"), None)
                .expect("valid envelope");
            assert_eq!(envelope.operation.attempt.get(), 1);
            serde_json::to_vec(&envelope).expect("envelope serializes")
        }

        assert_eq!(encoded(42), encoded(42));
        assert_ne!(encoded(42), encoded(43));
    }
}
