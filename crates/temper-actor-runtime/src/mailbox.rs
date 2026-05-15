//! Mailbox trait — the low-level messaging interface.
//!
//! Used internally by ActorContext and externally by the API layer
//! to send messages to actors.

use std::time::Duration;

use crate::actor::{ActorHandle, Message};

/// Errors from mailbox operations.
#[derive(Debug, thiserror::Error)]
pub enum MailboxError {
    #[error("storage error: {0}")]
    Storage(String),

    #[error("ask timeout after {0:?}")]
    Timeout(Duration),

    #[error("ask cancelled")]
    Cancelled,
}

/// The mailbox interface for sending messages to actors.
#[async_trait::async_trait]
pub trait Mailbox: Send + Sync + 'static {
    /// Fire-and-forget: deliver a message to an actor's mailbox.
    /// Returns the assigned message ID.
    async fn tell(
        &self,
        from: Option<&ActorHandle>,
        to: &ActorHandle,
        message_type: &str,
        payload: Vec<u8>,
    ) -> Result<i64, MailboxError>;

    /// Request-response: send a message and wait for a reply.
    ///
    /// Generates a correlation_id, inserts the message, then polls
    /// for a response with matching correlation_id.
    ///
    /// Poll interval: configurable on the implementation.
    /// Timeout: required parameter — how long to wait for a response.
    async fn ask(
        &self,
        from: &ActorHandle,
        to: &ActorHandle,
        message_type: &str,
        payload: Vec<u8>,
        timeout: Duration,
    ) -> Result<Message, MailboxError>;
}
