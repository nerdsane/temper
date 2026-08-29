//! Structured reaction-fanout source classification.

use crate::trigger::ReactionFailureKind;

pub(super) fn reaction_authorization_failure(
    denial: &temper_authz::AuthzDenial,
) -> ReactionFailureKind {
    match denial {
        temper_authz::AuthzDenial::PolicyDenied { .. }
        | temper_authz::AuthzDenial::NoMatchingPermit => ReactionFailureKind::AuthorizationDenied,
        temper_authz::AuthzDenial::InvalidPrincipal(_)
        | temper_authz::AuthzDenial::InvalidAction(_)
        | temper_authz::AuthzDenial::InvalidResource(_)
        | temper_authz::AuthzDenial::InvalidContext(_) => {
            ReactionFailureKind::AuthorizationContextInvalid
        }
        temper_authz::AuthzDenial::EngineError(_) => {
            ReactionFailureKind::AuthorizationEngineUnavailable
        }
    }
}

pub(super) fn reaction_authorization_decision_id(
    denial: &temper_authz::AuthzDenial,
) -> Option<String> {
    match denial {
        temper_authz::AuthzDenial::PolicyDenied { policy_ids } => {
            let mut policy_ids = policy_ids.clone();
            policy_ids.sort();
            policy_ids.dedup();
            Some(if policy_ids.is_empty() {
                "cedar:policy-denied".to_string()
            } else {
                format!("cedar:policies:{}", policy_ids.join(","))
            })
        }
        temper_authz::AuthzDenial::NoMatchingPermit => Some("cedar:no-matching-permit".to_string()),
        temper_authz::AuthzDenial::InvalidPrincipal(_)
        | temper_authz::AuthzDenial::InvalidAction(_)
        | temper_authz::AuthzDenial::InvalidResource(_)
        | temper_authz::AuthzDenial::InvalidContext(_)
        | temper_authz::AuthzDenial::EngineError(_) => None,
    }
}

#[allow(deprecated)]
pub(super) fn reaction_dispatch_failure(
    error: &crate::state::DispatchError,
) -> ReactionFailureKind {
    use crate::state::DispatchError;
    use temper_runtime::actor::ActorError;

    match error {
        DispatchError::Transient { source, .. } => match source {
            ActorError::MailboxFull => ReactionFailureKind::MailboxCapacityExhausted,
            ActorError::AskTimeout(_) => ReactionFailureKind::AcknowledgementLost,
            ActorError::Stopped
            | ActorError::SendFailed
            | ActorError::Panicked(_)
            | ActorError::InitFailed(_)
            | ActorError::MaxRestartsExceeded(_)
            | ActorError::Custom(_) => ReactionFailureKind::LegacyDispatchFailure,
        },
        DispatchError::Permanent { .. } => ReactionFailureKind::ActorPermanentlyUnavailable,
        DispatchError::Deferred { .. } => ReactionFailureKind::DispatchDeferred,
        DispatchError::AuthzDenied(_) => ReactionFailureKind::AuthorizationDenied,
        DispatchError::QuotaExceeded(_) => ReactionFailureKind::DispatchBudgetExhausted,
        DispatchError::Conflict(_) | DispatchError::CollectionWorkflowConflict(_) => {
            ReactionFailureKind::DispatchConflict
        }
        DispatchError::Ungoverned(_) => ReactionFailureKind::TargetUngoverned,
        DispatchError::ActorFailed(_)
        | DispatchError::WasmFailed(_)
        | DispatchError::Internal(_) => ReactionFailureKind::LegacyDispatchFailure,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use temper_authz::AuthzDenial;
    use temper_runtime::actor::ActorError;

    #[test]
    fn authorization_source_variants_are_exhaustively_mapped() {
        let cases = [
            (
                AuthzDenial::PolicyDenied {
                    policy_ids: vec!["policy-b".into(), "policy-a".into()],
                },
                ReactionFailureKind::AuthorizationDenied,
                Some("cedar:policies:policy-a,policy-b"),
            ),
            (
                AuthzDenial::NoMatchingPermit,
                ReactionFailureKind::AuthorizationDenied,
                Some("cedar:no-matching-permit"),
            ),
            (
                AuthzDenial::InvalidPrincipal("x".into()),
                ReactionFailureKind::AuthorizationContextInvalid,
                None,
            ),
            (
                AuthzDenial::InvalidAction("x".into()),
                ReactionFailureKind::AuthorizationContextInvalid,
                None,
            ),
            (
                AuthzDenial::InvalidResource("x".into()),
                ReactionFailureKind::AuthorizationContextInvalid,
                None,
            ),
            (
                AuthzDenial::InvalidContext("x".into()),
                ReactionFailureKind::AuthorizationContextInvalid,
                None,
            ),
            (
                AuthzDenial::EngineError("x".into()),
                ReactionFailureKind::AuthorizationEngineUnavailable,
                None,
            ),
        ];

        for (denial, expected_kind, expected_decision) in cases {
            assert_eq!(reaction_authorization_failure(&denial), expected_kind);
            assert_eq!(
                reaction_authorization_decision_id(&denial).as_deref(),
                expected_decision
            );
        }
    }

    #[test]
    #[allow(deprecated)]
    fn dispatch_and_actor_source_variants_are_exhaustively_mapped() {
        use crate::state::DispatchError;

        let actor_cases = [
            (
                ActorError::MailboxFull,
                ReactionFailureKind::MailboxCapacityExhausted,
            ),
            (
                ActorError::AskTimeout(Duration::from_secs(1)),
                ReactionFailureKind::AcknowledgementLost,
            ),
            (
                ActorError::Stopped,
                ReactionFailureKind::LegacyDispatchFailure,
            ),
            (
                ActorError::SendFailed,
                ReactionFailureKind::LegacyDispatchFailure,
            ),
            (
                ActorError::Panicked("x".into()),
                ReactionFailureKind::LegacyDispatchFailure,
            ),
            (
                ActorError::InitFailed("x".into()),
                ReactionFailureKind::LegacyDispatchFailure,
            ),
            (
                ActorError::MaxRestartsExceeded(1),
                ReactionFailureKind::LegacyDispatchFailure,
            ),
            (
                ActorError::custom("x"),
                ReactionFailureKind::LegacyDispatchFailure,
            ),
        ];
        for (source, expected) in actor_cases {
            assert_eq!(
                reaction_dispatch_failure(&DispatchError::Transient {
                    source,
                    attempts: 1,
                }),
                expected
            );
        }

        let dispatch_cases = [
            (
                DispatchError::Permanent {
                    source: ActorError::Stopped,
                },
                ReactionFailureKind::ActorPermanentlyUnavailable,
            ),
            (
                DispatchError::Deferred { retry_after_ms: 1 },
                ReactionFailureKind::DispatchDeferred,
            ),
            (
                DispatchError::AuthzDenied("x".into()),
                ReactionFailureKind::AuthorizationDenied,
            ),
            (
                DispatchError::QuotaExceeded("x".into()),
                ReactionFailureKind::DispatchBudgetExhausted,
            ),
            (
                DispatchError::Conflict("x".into()),
                ReactionFailureKind::DispatchConflict,
            ),
            (
                DispatchError::Ungoverned("x".into()),
                ReactionFailureKind::TargetUngoverned,
            ),
            (
                DispatchError::ActorFailed("x".into()),
                ReactionFailureKind::LegacyDispatchFailure,
            ),
            (
                DispatchError::WasmFailed("x".into()),
                ReactionFailureKind::LegacyDispatchFailure,
            ),
            (
                DispatchError::Internal("x".into()),
                ReactionFailureKind::LegacyDispatchFailure,
            ),
        ];
        for (source, expected) in dispatch_cases {
            assert_eq!(reaction_dispatch_failure(&source), expected);
        }
    }
}
