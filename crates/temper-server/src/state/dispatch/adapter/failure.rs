//! Typed application-failure adaptation for native adapters.

use temper_failure::{
    FailureCategory, FailureContractError, FailureEnvelopeV1, FailureOutcome, FailureProvenanceV1,
    FailureRetryability, FailureSource, ProvenanceToken, StableFailureCode,
};

use crate::adapters::AdapterError;

/// Structured failures before a credential-requiring adapter is invoked.
#[derive(Debug, thiserror::Error)]
pub(super) enum CredentialMintFailure {
    /// The caller lacks credential delegation authority or security context.
    #[error("{0}")]
    Authorization(String),
    /// Credential construction or a known rejected Issue result was invalid.
    #[error("{0}")]
    Integrity(String),
    /// Credential Issue may have committed before its acknowledgement failed.
    #[error("{0}")]
    DispatchAmbiguous(String),
}

impl CredentialMintFailure {
    pub(super) fn diagnostic(&self) -> &str {
        match self {
            Self::Authorization(diagnostic)
            | Self::Integrity(diagnostic)
            | Self::DispatchAmbiguous(diagnostic) => diagnostic,
        }
    }
}

/// Structured terminal failure at the native-adapter boundary.
pub(super) enum AdapterFailure {
    /// A configured adapter key was absent from the registry.
    MissingRegistry(String),
    /// A typed adapter execution error.
    Typed(AdapterError),
    /// A structured credential preparation failure.
    Credential(CredentialMintFailure),
    /// An unsuccessful legacy result without a typed source variant.
    Legacy(String),
}

impl AdapterFailure {
    /// Return diagnostic text for legacy callbacks and human debugging only.
    pub(super) fn diagnostic(&self) -> String {
        match self {
            Self::MissingRegistry(diagnostic) | Self::Legacy(diagnostic) => diagnostic.clone(),
            Self::Typed(error) => error.to_string(),
            Self::Credential(error) => error.diagnostic().to_string(),
        }
    }

    /// Adapt the structured source without reading diagnostic text.
    pub(super) fn into_envelope(
        self,
        causal_id: Option<&str>,
        scope: [&str; 5],
    ) -> Result<FailureEnvelopeV1, FailureContractError> {
        let operation = super::super::typed_failure::integration_operation(
            "adapter",
            "adapter.invoke",
            causal_id,
            scope,
        )?;
        let (category, code, retryability, outcome, source, source_code, diagnostic) = match self {
            Self::MissingRegistry(diagnostic) => (
                FailureCategory::Integrity,
                "AdapterNotRegistered",
                FailureRetryability::AfterRefresh,
                FailureOutcome::NotApplied,
                FailureSource::ExternalOperation,
                "MissingRegistry",
                diagnostic,
            ),
            Self::Typed(AdapterError::Invocation(diagnostic)) => (
                FailureCategory::Transient,
                "AdapterInvocationUnavailable",
                FailureRetryability::WithBackoff,
                FailureOutcome::NotApplied,
                FailureSource::ExternalOperation,
                "Invocation",
                diagnostic,
            ),
            Self::Typed(AdapterError::Execution(diagnostic)) => (
                FailureCategory::Ambiguous,
                "AdapterExecutionFailed",
                FailureRetryability::Reconcile,
                FailureOutcome::Unknown,
                FailureSource::ExternalOperation,
                "Execution",
                diagnostic,
            ),
            Self::Typed(AdapterError::Parse(diagnostic)) => (
                FailureCategory::Ambiguous,
                "AdapterResultInvalid",
                FailureRetryability::Reconcile,
                FailureOutcome::Unknown,
                FailureSource::ExternalOperation,
                "Parse",
                diagnostic,
            ),
            Self::Credential(CredentialMintFailure::Authorization(diagnostic)) => (
                FailureCategory::Authorization,
                "AdapterCredentialDenied",
                FailureRetryability::AfterAuthorization,
                FailureOutcome::NotApplied,
                FailureSource::Authorization,
                "CredentialDelegationDenied",
                diagnostic,
            ),
            Self::Credential(CredentialMintFailure::Integrity(diagnostic)) => (
                FailureCategory::Integrity,
                "AdapterCredentialInvalid",
                FailureRetryability::AfterRefresh,
                FailureOutcome::NotApplied,
                FailureSource::ExternalOperation,
                "CredentialPreparation",
                diagnostic,
            ),
            Self::Credential(CredentialMintFailure::DispatchAmbiguous(diagnostic)) => (
                FailureCategory::Ambiguous,
                "AdapterCredentialIssueUnknown",
                FailureRetryability::Reconcile,
                FailureOutcome::Unknown,
                FailureSource::ExternalOperation,
                "CredentialDispatch",
                diagnostic,
            ),
            Self::Legacy(diagnostic) => (
                FailureCategory::Permanent,
                "LegacyFreeFormFailure",
                FailureRetryability::Reconcile,
                FailureOutcome::Unknown,
                FailureSource::Legacy,
                "LegacyAdapterFailure",
                diagnostic,
            ),
        };
        let provenance = FailureProvenanceV1 {
            source,
            component: ProvenanceToken::new("native-adapter")?,
            source_code: Some(ProvenanceToken::new(source_code)?),
        };
        Ok(FailureEnvelopeV1::new(
            category,
            StableFailureCode::new(code)?,
            retryability,
            outcome,
            operation,
            provenance,
        )?
        .with_diagnostic(diagnostic))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(integration: &'static str) -> [&'static str; 5] {
        ["default", "Payment", "payment-1", "Charge", integration]
    }

    #[test]
    fn every_typed_adapter_variant_has_explicit_outcome_semantics() {
        let invocation = AdapterFailure::Typed(AdapterError::Invocation("diagnostic".into()))
            .into_envelope(Some("dispatch:adapter:invocation"), scope("invocation"))
            .expect("valid envelope");
        assert_eq!(invocation.category, FailureCategory::Transient);
        assert_eq!(invocation.outcome, FailureOutcome::NotApplied);

        for error in [
            AdapterError::Execution("diagnostic".into()),
            AdapterError::Parse("diagnostic".into()),
        ] {
            let envelope = AdapterFailure::Typed(error)
                .into_envelope(Some("dispatch:adapter:execution"), scope("execution"))
                .expect("valid envelope");
            assert_eq!(envelope.category, FailureCategory::Ambiguous);
            assert_eq!(envelope.outcome, FailureOutcome::Unknown);
            assert_eq!(envelope.retryability, FailureRetryability::Reconcile);
        }

        let authorization =
            AdapterFailure::Credential(CredentialMintFailure::Authorization("diagnostic".into()))
                .into_envelope(
                    Some("dispatch:credential:authorization"),
                    scope("credential-auth"),
                )
                .expect("valid envelope");
        assert_eq!(authorization.category, FailureCategory::Authorization);
        assert_eq!(authorization.outcome, FailureOutcome::NotApplied);

        let integrity =
            AdapterFailure::Credential(CredentialMintFailure::Integrity("diagnostic".into()))
                .into_envelope(
                    Some("dispatch:credential:integrity"),
                    scope("credential-integrity"),
                )
                .expect("valid envelope");
        assert_eq!(integrity.category, FailureCategory::Integrity);
        assert_eq!(integrity.outcome, FailureOutcome::NotApplied);

        let ambiguous = AdapterFailure::Credential(CredentialMintFailure::DispatchAmbiguous(
            "diagnostic".into(),
        ))
        .into_envelope(
            Some("dispatch:credential:ambiguous"),
            scope("credential-ambiguous"),
        )
        .expect("valid envelope");
        assert_eq!(ambiguous.category, FailureCategory::Ambiguous);
        assert_eq!(ambiguous.outcome, FailureOutcome::Unknown);
        assert_eq!(ambiguous.retryability, FailureRetryability::Reconcile);
    }

    #[test]
    fn diagnostic_wording_does_not_select_control_flow() {
        let first = AdapterFailure::Legacy("authorization denied".into())
            .into_envelope(Some("dispatch:adapter:legacy:1"), scope("legacy-1"))
            .expect("valid envelope");
        let second = AdapterFailure::Legacy("temporary network error".into())
            .into_envelope(Some("dispatch:adapter:legacy:2"), scope("legacy-2"))
            .expect("valid envelope");
        assert_eq!(first.category, FailureCategory::Permanent);
        assert_eq!(first.category, second.category);
    }
}
