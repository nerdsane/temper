//! The typed error of an action dispatch, and which of its answers are final.

/// Typed error enum for action dispatch failures.
///
/// Replaces bare `String` errors in the dispatch chain with structured
/// variants that preserve error context and enable pattern matching at
/// the HTTP boundary.
///
/// See ADR-0048 (Dispatch retry and error taxonomy) for the classification
/// contract. `Transient` and `Permanent` are the two error shapes an ask
/// site can produce after retry exhaustion; `Deferred` is reserved for
/// ADR-0051 (Admission control).
#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    /// The actor was reachable but retry budget was exhausted. The caller
    /// (or intermediary proxy) may retry with a fresh budget.
    ///
    /// `attempts` is how many tries were made before giving up; useful for
    /// metrics and log correlation.
    #[error("actor dispatch exhausted after {attempts} attempt(s): {source}")]
    Transient {
        source: temper_runtime::actor::ActorError,
        attempts: u32,
    },

    /// The actor returned a permanent error — retrying will not help.
    /// Includes stopped / panicked / init-failed / etc.
    #[error("actor dispatch permanently failed: {source}")]
    Permanent {
        source: temper_runtime::actor::ActorError,
    },

    /// Admission control denied the call and the caller should retry
    /// after `retry_after_ms` (ADR-0051).
    #[error("dispatch deferred: retry after {retry_after_ms}ms")]
    #[allow(dead_code)] // ADR-0051 will populate this from the admission layer
    Deferred { retry_after_ms: u64 },

    /// Legacy: generic actor failure as a string. Retained while callers
    /// migrate to `Transient`/`Permanent`. Prefer the classified variants.
    #[deprecated(note = "Use DispatchError::Transient or DispatchError::Permanent (ADR-0048)")]
    #[error("actor dispatch failed: {0}")]
    ActorFailed(String),

    /// A WASM integration invocation or callback failed.
    #[error("wasm integration failed: {0}")]
    #[allow(dead_code)] // Reserved for structured error handling migration
    WasmFailed(String),

    /// An authorization check denied the action.
    #[error("authorization denied: {0}")]
    #[allow(dead_code)] // Reserved for structured error handling migration
    AuthzDenied(String),

    /// A tenant or owner quota denied the action before dispatch.
    #[error("quota exceeded: {0}")]
    QuotaExceeded(String),

    /// The request conflicts with an existing governed row.
    #[error("conflict: {0}")]
    Conflict(String),

    /// The record changed after an outside write read it; the caller re-reads and retries.
    #[error("{0}")]
    StaleWrite(super::StaleWrite),

    /// The entity type has no registered spec — all actions are denied by default.
    #[error("entity type '{0}' is not governed by any registered spec")]
    Ungoverned(String),

    /// The store could not take the write: it failed, or its disk is under
    /// the floor. Answered 503, so the caller tries again.
    #[error("{0}")]
    StoreUnavailable(String),

    /// An internal error (serialization, persistence, unexpected state).
    #[error("{0}")]
    Internal(String),
}

impl DispatchError {
    /// Classify an `ActorError` into the appropriate `DispatchError` variant
    /// based on ADR-0048's transient/permanent taxonomy.
    ///
    /// `attempts` should be the number of retry attempts made before giving
    /// up (1 if no retry was attempted).
    pub(crate) fn from_actor_error(err: temper_runtime::actor::ActorError, attempts: u32) -> Self {
        if err.is_transient() {
            Self::Transient {
                source: err,
                attempts,
            }
        } else {
            Self::Permanent { source: err }
        }
    }

    /// Whether this answer is final for the move: the same move, sent again
    /// as the same caller, gets the same answer, so a caller that retries on
    /// its own (a scheduled fire) handles it as a refusal instead.
    ///
    /// Final, because what decides it does not change between tries:
    /// - `AuthzDenied`: decided by the caller's identity, the action's
    ///   declaration or the move's own parameters, all fixed for a retried
    ///   move (no security context, no caller identity for an
    ///   `authenticated_subject` parameter, a runtime-bound parameter
    ///   supplied, a `[[carry]]` move, a Cedar denial). A denial is a
    ///   decision like a guard refusal; a policy that later permits the move
    ///   is a reason for a new move, not for retrying this one.
    /// - `Ungoverned`: no spec governs the type, so no move on it can run.
    ///
    /// Not final, because a retry can get another answer:
    /// - `Transient`, `Deferred`, `StoreUnavailable`: the actor, admission or
    ///   the store was busy or down.
    /// - `Permanent`: permanent for the actor that answered (it stopped,
    ///   panicked or could not start), not for the move; the next try starts
    ///   the actor again.
    /// - `Conflict`: another write moved the records first (a concurrent
    ///   batch, a stale reference); the next try reads them again.
    /// - `QuotaExceeded`: room comes back when the owner frees storage or
    ///   reservations end.
    /// - `Internal`, `WasmFailed`, `ActorFailed`: not classified, so not
    ///   known to be final; dropping a move that could have landed loses it.
    ///
    /// Unknown actions and parameters that fail the action's contract are
    /// not errors here: the actor answers them as refusals.
    pub fn is_final(&self) -> bool {
        #[allow(deprecated)] // ActorFailed is classified like every variant
        match self {
            Self::AuthzDenied(_) | Self::Ungoverned(_) => true,
            Self::Transient { .. }
            | Self::Permanent { .. }
            | Self::Deferred { .. }
            | Self::ActorFailed(_)
            | Self::WasmFailed(_)
            | Self::QuotaExceeded(_)
            | Self::StaleWrite(_)
            | Self::Conflict(_)
            | Self::StoreUnavailable(_)
            | Self::Internal(_) => false,
        }
    }
}

impl From<String> for DispatchError {
    fn from(s: String) -> Self {
        Self::Internal(s)
    }
}

#[cfg(test)]
mod dispatch_error_tests {
    use super::*;
    use std::time::Duration;
    use temper_runtime::actor::ActorError;

    #[test]
    fn transient_actor_errors_map_to_transient_dispatch_variant() {
        let transient = ActorError::AskTimeout(Duration::from_secs(5));
        match DispatchError::from_actor_error(transient, 3) {
            DispatchError::Transient { attempts, .. } => assert_eq!(attempts, 3),
            other => panic!("expected Transient, got {other:?}"),
        }
        let mailbox = ActorError::MailboxFull;
        match DispatchError::from_actor_error(mailbox, 1) {
            DispatchError::Transient { attempts, .. } => assert_eq!(attempts, 1),
            other => panic!("expected Transient, got {other:?}"),
        }
    }

    #[test]
    fn permanent_actor_errors_map_to_permanent_dispatch_variant() {
        for err in [
            ActorError::Stopped,
            ActorError::SendFailed,
            ActorError::Panicked("boom".into()),
            ActorError::InitFailed("init".into()),
            ActorError::MaxRestartsExceeded(4),
            ActorError::custom("misc"),
        ] {
            let label = format!("{err:?}");
            match DispatchError::from_actor_error(err, 7) {
                DispatchError::Permanent { .. } => {}
                other => panic!("expected Permanent for {label}, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_denial_and_an_ungoverned_type_are_final() {
        assert!(
            DispatchError::AuthzDenied("Authenticated subject requires a caller identity".into())
                .is_final()
        );
        assert!(DispatchError::Ungoverned("Gone".into()).is_final());
    }

    #[test]
    fn an_error_a_retry_can_change_is_not_final() {
        #[allow(deprecated)]
        let legacy = DispatchError::ActorFailed("legacy".into());
        for error in [
            DispatchError::from_actor_error(ActorError::AskTimeout(Duration::from_secs(5)), 3),
            DispatchError::from_actor_error(ActorError::MailboxFull, 3),
            DispatchError::from_actor_error(ActorError::Stopped, 1),
            DispatchError::from_actor_error(ActorError::Panicked("boom".into()), 1),
            DispatchError::Deferred {
                retry_after_ms: 100,
            },
            legacy,
            DispatchError::WasmFailed("module".into()),
            DispatchError::QuotaExceeded("over the cap".into()),
            DispatchError::Conflict("a newer row".into()),
            DispatchError::StoreUnavailable("the store takes no writes".into()),
            DispatchError::Internal("serialization".into()),
        ] {
            assert!(!error.is_final(), "{error:?}");
        }
    }
}
