//! Typed durable delivery failure adaptation (ADR-0187).

use temper_failure::{
    BoundedDetailString, CausalOperationV1, DetailKey, FailureCategory, FailureContractError,
    FailureDetailValue, FailureEnvelopeV1, FailureOutcome, FailureProvenanceV1,
    FailureRetryability, FailureSource, OperationAttempt, OperationId, OperationKind,
    ProvenanceToken, StableFailureCode,
};

use super::{DeliveryKind, PersistedReactionIntent};
use crate::trigger::types::ReactionFailureKind;

/// Structured terminal facts owned by the durable delivery subsystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DurableFailureKind {
    /// A typed failure reported by target fanout.
    Reaction(ReactionFailureKind),
    /// The bounded automatic delivery attempt budget was consumed.
    AutomaticAttemptBudgetExhausted,
    /// Persisted intent shape disagreed with its declared delivery kind.
    InvalidDeliveryShape,
    /// Persisted rule data failed validation.
    InvalidPersistedRule,
    /// The bounded reaction cascade depth was consumed.
    CascadeDepthBudgetExhausted,
    /// Persisted authority failed validation.
    InvalidPersistedAuthority,
    /// A stale timeout generation was cancelled before applying.
    TimeoutGenerationSuperseded,
    /// Timeout clock evidence failed validation.
    TimeoutClockInvalid,
    /// The declared timeout occurrence budget was consumed.
    TimeoutOccurrenceBudgetExhausted,
    /// Timeout clock auditing consumed its bounded event budget.
    TimeoutClockAuditBudgetExhausted,
}

/// Adapt one structured durable-delivery terminal fact into the canonical v1 envelope.
pub(crate) fn delivery_failure_envelope(
    intent: &PersistedReactionIntent,
    attempts: u32,
    kind: DurableFailureKind,
    diagnostic: Option<&str>,
    decision_id: Option<&str>,
) -> Result<FailureEnvelopeV1, FailureContractError> {
    let (category, code, retryability, outcome) = classification(kind);
    let source = if intent.kind == DeliveryKind::StateTimeout {
        FailureSource::Timeout
    } else {
        FailureSource::Reaction
    };
    let operation_kind = if source == FailureSource::Timeout {
        "state_timeout.deliver"
    } else {
        "reaction.deliver"
    };
    let parent_id = (intent.root_delivery_id != intent.delivery_id)
        .then(|| OperationId::new(intent.root_delivery_id.clone()))
        .transpose()?;
    assert!(
        attempts <= u32::from(temper_failure::MAX_OPERATION_ATTEMPT),
        "durable delivery attempts exceed failure-envelope budget"
    );
    let bounded_attempt = OperationAttempt::new(attempts as u16)?;
    let operation = CausalOperationV1 {
        id: OperationId::new(intent.delivery_id.clone())?,
        kind: OperationKind::new(operation_kind)?,
        attempt: bounded_attempt,
        parent_id,
    };
    let provenance = FailureProvenanceV1 {
        source,
        component: ProvenanceToken::new("temper-server.delivery")?,
        source_code: Some(ProvenanceToken::new(code)?),
    };
    let mut envelope = FailureEnvelopeV1::new(
        category,
        StableFailureCode::new(code)?,
        retryability,
        outcome,
        operation,
        provenance,
    )?;
    if let Some(decision_id) = decision_id {
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
    if let Some(message) = diagnostic {
        envelope = envelope.with_diagnostic(message);
    }
    Ok(envelope)
}

fn classification(
    kind: DurableFailureKind,
) -> (
    FailureCategory,
    &'static str,
    FailureRetryability,
    FailureOutcome,
) {
    use FailureCategory::{Ambiguous, Authorization, Budget, Integrity, Permanent, Transient};
    use FailureOutcome::{Applied, NotApplied, Unknown};
    use FailureRetryability::{AfterAuthorization, AfterRefresh, Never, Reconcile, WithBackoff};
    use ReactionFailureKind as Reaction;

    match kind {
        DurableFailureKind::Reaction(Reaction::TargetResolution) => {
            (Integrity, "ReactionTargetUnresolved", Never, NotApplied)
        }
        DurableFailureKind::Reaction(Reaction::TargetSnapshotUnavailable) => (
            Transient,
            "ReactionTargetSnapshotUnavailable",
            WithBackoff,
            NotApplied,
        ),
        DurableFailureKind::Reaction(Reaction::AuthorizationDenied) => (
            Authorization,
            "ReactionAuthorizationDenied",
            AfterAuthorization,
            NotApplied,
        ),
        DurableFailureKind::Reaction(Reaction::AuthorizationContextInvalid) => (
            Integrity,
            "ReactionAuthorizationContextInvalid",
            Never,
            NotApplied,
        ),
        DurableFailureKind::Reaction(Reaction::AuthorizationEngineUnavailable) => (
            Transient,
            "ReactionAuthorizationEngineUnavailable",
            WithBackoff,
            NotApplied,
        ),
        DurableFailureKind::Reaction(Reaction::TargetTransitionRejected) => (
            Integrity,
            "ReactionTargetTransitionRejected",
            AfterRefresh,
            NotApplied,
        ),
        DurableFailureKind::Reaction(Reaction::MailboxCapacityExhausted) => (
            Transient,
            "ReactionMailboxCapacityExhausted",
            WithBackoff,
            NotApplied,
        ),
        DurableFailureKind::Reaction(Reaction::AcknowledgementLost) => {
            (Ambiguous, "ReactionAcknowledgementLost", Reconcile, Unknown)
        }
        DurableFailureKind::Reaction(Reaction::DispatchDeferred) => (
            Transient,
            "ReactionDispatchDeferred",
            WithBackoff,
            NotApplied,
        ),
        DurableFailureKind::Reaction(Reaction::DispatchBudgetExhausted) => {
            (Budget, "ReactionDispatchBudgetExhausted", Never, NotApplied)
        }
        DurableFailureKind::Reaction(Reaction::DispatchConflict) => (
            Integrity,
            "ReactionDispatchConflict",
            AfterRefresh,
            NotApplied,
        ),
        DurableFailureKind::Reaction(Reaction::TargetUngoverned) => {
            (Integrity, "ReactionTargetUngoverned", Never, NotApplied)
        }
        DurableFailureKind::Reaction(Reaction::ActorPermanentlyUnavailable) => (
            Permanent,
            "ReactionActorPermanentlyUnavailable",
            Never,
            NotApplied,
        ),
        DurableFailureKind::Reaction(Reaction::LegacyDispatchFailure) => (
            Permanent,
            "LegacyReactionDispatchFailure",
            Never,
            NotApplied,
        ),
        DurableFailureKind::Reaction(Reaction::PostCommitDescendantFailure) => (
            Transient,
            "ReactionPostCommitDescendantFailure",
            WithBackoff,
            Applied,
        ),
        DurableFailureKind::AutomaticAttemptBudgetExhausted => {
            (Budget, "DeliveryAttemptBudgetExhausted", Never, NotApplied)
        }
        DurableFailureKind::InvalidDeliveryShape => {
            (Integrity, "InvalidDeliveryShape", Never, NotApplied)
        }
        DurableFailureKind::InvalidPersistedRule => {
            (Integrity, "InvalidPersistedReactionRule", Never, NotApplied)
        }
        DurableFailureKind::CascadeDepthBudgetExhausted => (
            Budget,
            "ReactionCascadeDepthBudgetExhausted",
            Never,
            NotApplied,
        ),
        DurableFailureKind::InvalidPersistedAuthority => (
            Integrity,
            "InvalidPersistedReactionAuthority",
            Never,
            NotApplied,
        ),
        DurableFailureKind::TimeoutGenerationSuperseded => (
            Integrity,
            "StateTimeoutGenerationSuperseded",
            Never,
            NotApplied,
        ),
        DurableFailureKind::TimeoutClockInvalid => {
            (Integrity, "StateTimeoutClockInvalid", Never, NotApplied)
        }
        DurableFailureKind::TimeoutOccurrenceBudgetExhausted => (
            Budget,
            "StateTimeoutOccurrenceBudgetExhausted",
            Never,
            NotApplied,
        ),
        DurableFailureKind::TimeoutClockAuditBudgetExhausted => (
            Budget,
            "StateTimeoutClockAuditBudgetExhausted",
            Never,
            NotApplied,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use FailureCategory::{Ambiguous, Authorization, Budget, Integrity, Permanent, Transient};
    use FailureOutcome::{Applied, NotApplied, Unknown};
    use FailureRetryability::{AfterAuthorization, AfterRefresh, Never, Reconcile, WithBackoff};

    type Expected = (
        FailureCategory,
        &'static str,
        FailureRetryability,
        FailureOutcome,
    );

    #[test]
    fn reaction_failure_variants_have_exhaustive_envelope_semantics() {
        use ReactionFailureKind as Reaction;
        let cases: [(Reaction, Expected); 15] = [
            (
                Reaction::TargetResolution,
                (Integrity, "ReactionTargetUnresolved", Never, NotApplied),
            ),
            (
                Reaction::TargetSnapshotUnavailable,
                (
                    Transient,
                    "ReactionTargetSnapshotUnavailable",
                    WithBackoff,
                    NotApplied,
                ),
            ),
            (
                Reaction::AuthorizationDenied,
                (
                    Authorization,
                    "ReactionAuthorizationDenied",
                    AfterAuthorization,
                    NotApplied,
                ),
            ),
            (
                Reaction::AuthorizationContextInvalid,
                (
                    Integrity,
                    "ReactionAuthorizationContextInvalid",
                    Never,
                    NotApplied,
                ),
            ),
            (
                Reaction::AuthorizationEngineUnavailable,
                (
                    Transient,
                    "ReactionAuthorizationEngineUnavailable",
                    WithBackoff,
                    NotApplied,
                ),
            ),
            (
                Reaction::TargetTransitionRejected,
                (
                    Integrity,
                    "ReactionTargetTransitionRejected",
                    AfterRefresh,
                    NotApplied,
                ),
            ),
            (
                Reaction::MailboxCapacityExhausted,
                (
                    Transient,
                    "ReactionMailboxCapacityExhausted",
                    WithBackoff,
                    NotApplied,
                ),
            ),
            (
                Reaction::AcknowledgementLost,
                (Ambiguous, "ReactionAcknowledgementLost", Reconcile, Unknown),
            ),
            (
                Reaction::DispatchDeferred,
                (
                    Transient,
                    "ReactionDispatchDeferred",
                    WithBackoff,
                    NotApplied,
                ),
            ),
            (
                Reaction::DispatchBudgetExhausted,
                (Budget, "ReactionDispatchBudgetExhausted", Never, NotApplied),
            ),
            (
                Reaction::DispatchConflict,
                (
                    Integrity,
                    "ReactionDispatchConflict",
                    AfterRefresh,
                    NotApplied,
                ),
            ),
            (
                Reaction::TargetUngoverned,
                (Integrity, "ReactionTargetUngoverned", Never, NotApplied),
            ),
            (
                Reaction::ActorPermanentlyUnavailable,
                (
                    Permanent,
                    "ReactionActorPermanentlyUnavailable",
                    Never,
                    NotApplied,
                ),
            ),
            (
                Reaction::LegacyDispatchFailure,
                (
                    Permanent,
                    "LegacyReactionDispatchFailure",
                    Never,
                    NotApplied,
                ),
            ),
            (
                Reaction::PostCommitDescendantFailure,
                (
                    Transient,
                    "ReactionPostCommitDescendantFailure",
                    WithBackoff,
                    Applied,
                ),
            ),
        ];
        for (kind, expected) in cases {
            assert_eq!(classification(DurableFailureKind::Reaction(kind)), expected);
        }
    }

    #[test]
    fn durable_delivery_variants_have_exhaustive_envelope_semantics() {
        let cases: [(DurableFailureKind, Expected); 10] = [
            (
                DurableFailureKind::AutomaticAttemptBudgetExhausted,
                (Budget, "DeliveryAttemptBudgetExhausted", Never, NotApplied),
            ),
            (
                DurableFailureKind::InvalidDeliveryShape,
                (Integrity, "InvalidDeliveryShape", Never, NotApplied),
            ),
            (
                DurableFailureKind::InvalidPersistedRule,
                (Integrity, "InvalidPersistedReactionRule", Never, NotApplied),
            ),
            (
                DurableFailureKind::CascadeDepthBudgetExhausted,
                (
                    Budget,
                    "ReactionCascadeDepthBudgetExhausted",
                    Never,
                    NotApplied,
                ),
            ),
            (
                DurableFailureKind::InvalidPersistedAuthority,
                (
                    Integrity,
                    "InvalidPersistedReactionAuthority",
                    Never,
                    NotApplied,
                ),
            ),
            (
                DurableFailureKind::TimeoutGenerationSuperseded,
                (
                    Integrity,
                    "StateTimeoutGenerationSuperseded",
                    Never,
                    NotApplied,
                ),
            ),
            (
                DurableFailureKind::TimeoutClockInvalid,
                (Integrity, "StateTimeoutClockInvalid", Never, NotApplied),
            ),
            (
                DurableFailureKind::TimeoutOccurrenceBudgetExhausted,
                (
                    Budget,
                    "StateTimeoutOccurrenceBudgetExhausted",
                    Never,
                    NotApplied,
                ),
            ),
            (
                DurableFailureKind::TimeoutClockAuditBudgetExhausted,
                (
                    Budget,
                    "StateTimeoutClockAuditBudgetExhausted",
                    Never,
                    NotApplied,
                ),
            ),
            (
                DurableFailureKind::Reaction(ReactionFailureKind::AcknowledgementLost),
                (Ambiguous, "ReactionAcknowledgementLost", Reconcile, Unknown),
            ),
        ];
        for (kind, expected) in cases {
            assert_eq!(classification(kind), expected);
        }
    }
}
