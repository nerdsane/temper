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
