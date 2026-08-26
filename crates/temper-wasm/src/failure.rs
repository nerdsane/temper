//! Structured adaptation of WASM engine failures.

use temper_failure::{
    CausalOperationV1, DetailKey, FailureCategory, FailureContractError, FailureDetailValue,
    FailureEnvelopeV1, FailureOutcome, FailureProvenanceV1, FailureRetryability, FailureSource,
    ProvenanceToken, StableFailureCode,
};

use crate::WasmError;

/// Adapt a WASM engine error without using its display text for classification.
///
/// Errors that may occur after guest execution begins have an unknown outcome.
/// Callers must reconcile those operations rather than automatically replaying them.
pub fn adapt_wasm_error(
    error: &WasmError,
    operation: CausalOperationV1,
) -> Result<FailureEnvelopeV1, FailureContractError> {
    let (category, code, retryability, outcome) = classification(error);
    let source_code = match error {
        WasmError::InvalidGuestResult(kind) => kind.source_code(),
        _ => code,
    };
    let component = match error {
        WasmError::InvalidGuestResult(_) => "wasm-result-validator",
        _ => "wasm-engine",
    };
    let provenance = FailureProvenanceV1 {
        source: FailureSource::Wasm,
        component: ProvenanceToken::new(component)?,
        source_code: Some(ProvenanceToken::new(source_code)?),
    };
    let mut envelope = FailureEnvelopeV1::new(
        category,
        StableFailureCode::new(code)?,
        retryability,
        outcome,
        operation,
        provenance,
    )?
    .with_diagnostic(error.to_string());

    match error {
        WasmError::ModuleTooLarge { size, max } => {
            insert_u64(&mut envelope, "actual_bytes", usize_to_u64(*size));
            insert_u64(&mut envelope, "budget_bytes", usize_to_u64(*max));
        }
        WasmError::Timeout(duration) => {
            insert_u64(
                &mut envelope,
                "budget_millis",
                u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
            );
        }
        WasmError::MemoryLimitExceeded { max_bytes } => {
            insert_u64(&mut envelope, "budget_bytes", usize_to_u64(*max_bytes));
        }
        WasmError::Compilation(_)
        | WasmError::Instantiation(_)
        | WasmError::Invocation(_)
        | WasmError::GuestExecution(_)
        | WasmError::InvalidGuestResult(_)
        | WasmError::FuelExhausted
        | WasmError::ModuleNotFound(_) => {}
    }
    Ok(envelope)
}

fn classification(
    error: &WasmError,
) -> (
    FailureCategory,
    &'static str,
    FailureRetryability,
    FailureOutcome,
) {
    match error {
        WasmError::ModuleTooLarge { .. } => (
            FailureCategory::Budget,
            "WasmModuleTooLarge",
            FailureRetryability::Never,
            FailureOutcome::NotApplied,
        ),
        WasmError::Compilation(_) => (
            FailureCategory::Integrity,
            "WasmCompilationFailed",
            FailureRetryability::Never,
            FailureOutcome::NotApplied,
        ),
        WasmError::Instantiation(_) => (
            FailureCategory::Integrity,
            "WasmInstantiationFailed",
            FailureRetryability::Never,
            FailureOutcome::NotApplied,
        ),
        WasmError::ModuleNotFound(_) => (
            FailureCategory::Integrity,
            "WasmModuleNotFound",
            FailureRetryability::AfterRefresh,
            FailureOutcome::NotApplied,
        ),
        WasmError::Invocation(_) => (
            FailureCategory::Integrity,
            "WasmPreparationFailed",
            FailureRetryability::Never,
            FailureOutcome::NotApplied,
        ),
        WasmError::GuestExecution(_) => ambiguous("WasmGuestExecutionFailed"),
        WasmError::InvalidGuestResult(_) => ambiguous("InvalidGuestFailureResult"),
        WasmError::FuelExhausted => ambiguous("WasmFuelExhausted"),
        WasmError::Timeout(_) => ambiguous("WasmTimedOut"),
        WasmError::MemoryLimitExceeded { .. } => ambiguous("WasmMemoryBudgetExceeded"),
    }
}

fn ambiguous(
    code: &'static str,
) -> (
    FailureCategory,
    &'static str,
    FailureRetryability,
    FailureOutcome,
) {
    (
        FailureCategory::Ambiguous,
        code,
        FailureRetryability::Reconcile,
        FailureOutcome::Unknown,
    )
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn insert_u64(envelope: &mut FailureEnvelopeV1, key: &'static str, value: u64) {
    let key = DetailKey::new(key).expect("static detail keys satisfy the failure contract");
    envelope.insert_detail_or_omit(key, FailureDetailValue::Unsigned(value));
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use temper_failure::{OperationAttempt, OperationId, OperationKind};

    fn operation() -> CausalOperationV1 {
        CausalOperationV1 {
            id: OperationId::new("wasm:invocation:42").expect("valid operation id"),
            kind: OperationKind::new("wasm.invoke").expect("valid operation kind"),
            attempt: OperationAttempt::new(1).expect("valid attempt"),
            parent_id: None,
        }
    }

    #[test]
    fn pre_execution_failures_are_known_not_applied() {
        let cases = [
            WasmError::ModuleTooLarge { size: 2, max: 1 },
            WasmError::Compilation("invalid module".into()),
            WasmError::Instantiation("missing import".into()),
            WasmError::Invocation("failed before guest start".into()),
            WasmError::ModuleNotFound("hash".into()),
        ];
        for error in cases {
            let envelope = adapt_wasm_error(&error, operation()).expect("valid envelope");
            assert_eq!(envelope.outcome, FailureOutcome::NotApplied);
            assert_ne!(envelope.category, FailureCategory::Ambiguous);
        }
    }

    #[test]
    fn execution_failures_require_reconciliation() {
        let cases = [
            WasmError::GuestExecution("guest trapped".into()),
            WasmError::FuelExhausted,
            WasmError::Timeout(Duration::from_secs(2)),
            WasmError::MemoryLimitExceeded { max_bytes: 64 },
        ];
        for error in cases {
            let envelope = adapt_wasm_error(&error, operation()).expect("valid envelope");
            assert_eq!(envelope.category, FailureCategory::Ambiguous);
            assert_eq!(envelope.retryability, FailureRetryability::Reconcile);
            assert_eq!(envelope.outcome, FailureOutcome::Unknown);
        }
    }

    #[test]
    fn diagnostics_do_not_change_classification() {
        let first = adapt_wasm_error(
            &WasmError::GuestExecution("authorization denied".into()),
            operation(),
        )
        .expect("valid envelope");
        let second = adapt_wasm_error(
            &WasmError::GuestExecution("temporary network error".into()),
            operation(),
        )
        .expect("valid envelope");
        assert_eq!(first.category, second.category);
        assert_eq!(first.code, second.code);
        assert_eq!(first.retryability, second.retryability);
    }

    #[test]
    fn invalid_guest_results_use_the_pinned_ambiguous_envelope() {
        for kind in [
            crate::InvalidGuestResultKind::AbsentResult,
            crate::InvalidGuestResultKind::MultipleWrites,
            crate::InvalidGuestResultKind::MultipleSources,
            crate::InvalidGuestResultKind::InvalidLength,
            crate::InvalidGuestResultKind::ResultTooLarge,
            crate::InvalidGuestResultKind::OutOfBounds,
            crate::InvalidGuestResultKind::InvalidUtf8,
            crate::InvalidGuestResultKind::InvalidJson,
            crate::InvalidGuestResultKind::InvalidShape,
        ] {
            let envelope = adapt_wasm_error(&WasmError::InvalidGuestResult(kind), operation())
                .expect("invalid guest result should adapt");
            assert_eq!(envelope.category, FailureCategory::Ambiguous);
            assert_eq!(envelope.code.as_str(), "InvalidGuestFailureResult");
            assert_eq!(envelope.retryability, FailureRetryability::Reconcile);
            assert_eq!(envelope.outcome, FailureOutcome::Unknown);
            assert_eq!(envelope.provenance.source, FailureSource::Wasm);
            assert_eq!(
                envelope.provenance.component.as_str(),
                "wasm-result-validator"
            );
            assert_eq!(
                envelope
                    .provenance
                    .source_code
                    .as_ref()
                    .map(ProvenanceToken::as_str),
                Some(kind.source_code())
            );
        }
    }
}
