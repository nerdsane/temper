use thiserror::Error;

/// Why an actor could not initialize, from the caller's point of view.
///
/// `pre_start` failures are not interchangeable: a constraint rejection is the
/// caller's input being wrong, a store blip is the platform being briefly
/// unavailable, and anything else is a defect. Callers (HTTP mapping, dispatch
/// retry) need to tell them apart without parsing an error string, so the
/// classification travels with the error instead of being re-derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitFailureKind {
    /// A uniqueness / integrity constraint rejected the actor's bootstrap
    /// write. The same request will be rejected the same way forever — the
    /// caller must change its input. Maps to HTTP 409.
    Constraint,
    /// A dependency the actor needs to initialize (event store, network)
    /// failed in a way that may clear on its own. Maps to HTTP 503.
    TransientDependency,
    /// Anything else — a defect in the actor or its configuration. Retrying
    /// does not help. Maps to HTTP 500.
    Defect,
}

impl InitFailureKind {
    /// Stable label for metrics and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Constraint => "constraint",
            Self::TransientDependency => "transient_dependency",
            Self::Defect => "defect",
        }
    }
}

/// Errors that can occur during actor lifecycle and message handling.
#[derive(Error, Debug)]
pub enum ActorError {
    #[error("actor stopped")]
    Stopped,

    #[error("mailbox full")]
    MailboxFull,

    #[error("send failed: actor not running")]
    SendFailed,

    #[error("ask timeout after {0:?}")]
    AskTimeout(std::time::Duration),

    #[error("actor panicked: {0}")]
    Panicked(String),

    /// The actor could not initialize: `pre_start` returned an error and the
    /// supervision strategy gave up.
    ///
    /// Distinct from [`ActorError::Stopped`], which means "you asked an actor
    /// that had already finished". Conflating the two hides the cause: an
    /// `ask` whose reply channel is dropped by a dying cell reads as
    /// `Stopped`, which tells the caller nothing about *why* and cannot be
    /// classified. This variant carries the underlying cause verbatim and the
    /// classification the caller needs.
    #[error("actor init failed ({}): {cause}", kind.as_str())]
    InitFailed {
        /// The underlying failure, formatted at the point it happened.
        cause: String,
        /// How the caller should treat it.
        kind: InitFailureKind,
    },

    #[error("max restart attempts exceeded ({0})")]
    MaxRestartsExceeded(u32),

    #[error("{0}")]
    Custom(#[from] anyhow::Error),
}

impl ActorError {
    /// Create a custom error with a descriptive message.
    pub fn custom(msg: impl Into<String>) -> Self {
        Self::Custom(anyhow::anyhow!("{}", msg.into()))
    }

    /// Create an init failure carrying its cause and classification.
    pub fn init_failed(cause: impl Into<String>, kind: InitFailureKind) -> Self {
        Self::InitFailed {
            cause: cause.into(),
            kind,
        }
    }

    /// The init-failure classification, if this is an init failure.
    pub fn init_failure_kind(&self) -> Option<InitFailureKind> {
        match self {
            Self::InitFailed { kind, .. } => Some(*kind),
            _ => None,
        }
    }

    /// Returns `true` if retrying the operation may succeed because the cause
    /// is timing, capacity, or dependency related rather than logic.
    ///
    /// Transient variants:
    /// - `AskTimeout` — the actor did not reply within the per-attempt budget.
    /// - `MailboxFull` — the actor's bounded mailbox refused the message.
    /// - `InitFailed` with [`InitFailureKind::TransientDependency`] — the
    ///   actor could not start because a dependency was briefly unavailable.
    ///   Before this existed every init failure was permanent, so a store blip
    ///   during spawn was reported as a hard error and never retried.
    ///
    /// See ADR-0048 (Dispatch-layer retry and error taxonomy).
    pub fn is_transient(&self) -> bool {
        match self {
            Self::AskTimeout(_) | Self::MailboxFull => true,
            Self::InitFailed { kind, .. } => *kind == InitFailureKind::TransientDependency,
            _ => false,
        }
    }

    /// Returns `true` if the error reflects a terminal condition and retries
    /// are pointless. Every variant is classified as either transient or
    /// permanent, never both.
    ///
    /// See ADR-0048 (Dispatch-layer retry and error taxonomy).
    pub fn is_permanent(&self) -> bool {
        !self.is_transient()
    }
}

// Needed because anyhow::Error doesn't implement PartialEq
impl PartialEq for ActorError {
    fn eq(&self, other: &Self) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn error_display_messages() {
        assert_eq!(ActorError::Stopped.to_string(), "actor stopped");
        assert_eq!(ActorError::MailboxFull.to_string(), "mailbox full");
        assert_eq!(
            ActorError::SendFailed.to_string(),
            "send failed: actor not running"
        );
        assert_eq!(
            ActorError::AskTimeout(Duration::from_secs(5)).to_string(),
            "ask timeout after 5s"
        );
        assert_eq!(
            ActorError::Panicked("boom".to_string()).to_string(),
            "actor panicked: boom"
        );
        assert_eq!(
            ActorError::init_failed("init error", InitFailureKind::Defect).to_string(),
            "actor init failed (defect): init error"
        );
        assert_eq!(
            ActorError::init_failed(
                "duplicate declared key 'ws_path' for File: held by fl-1",
                InitFailureKind::Constraint
            )
            .to_string(),
            "actor init failed (constraint): duplicate declared key 'ws_path' for File: held by fl-1"
        );
        assert_eq!(
            ActorError::MaxRestartsExceeded(3).to_string(),
            "max restart attempts exceeded (3)"
        );
    }

    #[test]
    fn custom_error_from_string() {
        let err = ActorError::custom("test error");
        assert!(err.to_string().contains("test error"));
    }

    #[test]
    fn partial_eq_same_variant() {
        assert_eq!(ActorError::Stopped, ActorError::Stopped);
        assert_eq!(ActorError::MailboxFull, ActorError::MailboxFull);
    }

    #[test]
    fn partial_eq_different_variant() {
        assert_ne!(ActorError::Stopped, ActorError::MailboxFull);
    }

    #[test]
    fn transient_classification_is_exhaustive() {
        // Transient variants — retrying may succeed.
        assert!(ActorError::AskTimeout(Duration::from_secs(5)).is_transient());
        assert!(ActorError::MailboxFull.is_transient());
        assert!(
            ActorError::init_failed("store down", InitFailureKind::TransientDependency)
                .is_transient()
        );

        // Permanent variants — retrying is pointless.
        assert!(ActorError::Stopped.is_permanent());
        assert!(ActorError::SendFailed.is_permanent());
        assert!(ActorError::Panicked("x".into()).is_permanent());
        assert!(ActorError::init_failed("x", InitFailureKind::Defect).is_permanent());
        assert!(ActorError::init_failed("dup key", InitFailureKind::Constraint).is_permanent());
        assert!(ActorError::MaxRestartsExceeded(3).is_permanent());
        assert!(ActorError::custom("boom").is_permanent());
    }

    #[test]
    fn transient_and_permanent_are_mutually_exclusive() {
        // Every variant must be exactly one of transient or permanent.
        let all = [
            ActorError::Stopped,
            ActorError::MailboxFull,
            ActorError::SendFailed,
            ActorError::AskTimeout(Duration::from_millis(1)),
            ActorError::Panicked("p".into()),
            ActorError::init_failed("i", InitFailureKind::Defect),
            ActorError::init_failed("i", InitFailureKind::Constraint),
            ActorError::init_failed("i", InitFailureKind::TransientDependency),
            ActorError::MaxRestartsExceeded(1),
            ActorError::custom("c"),
        ];
        for err in all {
            assert_ne!(
                err.is_transient(),
                err.is_permanent(),
                "variant {err:?} is both (or neither) transient and permanent"
            );
        }
    }

    #[test]
    fn init_failure_is_distinct_from_stopped() {
        // The bug this taxonomy fixes: an ask against an actor that died in
        // pre_start used to be indistinguishable from an ask against an actor
        // that had already finished its work.
        let stopped = ActorError::Stopped;
        let init = ActorError::init_failed("boom", InitFailureKind::Constraint);
        assert_ne!(stopped, init);
        assert_eq!(stopped.init_failure_kind(), None);
        assert_eq!(init.init_failure_kind(), Some(InitFailureKind::Constraint));
    }

    #[test]
    fn init_failure_preserves_the_underlying_cause() {
        let err = ActorError::init_failed(
            "duplicate declared key 'ws_path' for File: held by fl-019efda8",
            InitFailureKind::Constraint,
        );
        // The operator-facing cause must survive to the caller verbatim —
        // that is the whole point of the variant.
        assert!(err.to_string().contains("ws_path"));
        assert!(err.to_string().contains("fl-019efda8"));
    }

    #[test]
    fn init_failure_kind_labels_are_stable() {
        assert_eq!(InitFailureKind::Constraint.as_str(), "constraint");
        assert_eq!(
            InitFailureKind::TransientDependency.as_str(),
            "transient_dependency"
        );
        assert_eq!(InitFailureKind::Defect.as_str(), "defect");
    }
}
