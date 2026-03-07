use thiserror::Error;

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

    #[error("actor init failed: {0}")]
    InitFailed(String),

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
        assert_eq!(ActorError::SendFailed.to_string(), "send failed: actor not running");
        assert_eq!(
            ActorError::AskTimeout(Duration::from_secs(5)).to_string(),
            "ask timeout after 5s"
        );
        assert_eq!(
            ActorError::Panicked("boom".to_string()).to_string(),
            "actor panicked: boom"
        );
        assert_eq!(
            ActorError::InitFailed("init error".to_string()).to_string(),
            "actor init failed: init error"
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
}
