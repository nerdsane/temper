use std::fmt;
use std::time::Duration;

use tokio::sync::oneshot;

use super::errors::ActorError;
use super::traits::Message;
use crate::mailbox::MailboxSender;

/// An envelope wrapping a message with an optional reply channel.
pub(crate) enum Envelope<M: Message> {
    /// Fire-and-forget message.
    Tell(M),
    /// Request-response message with a reply channel.
    Ask {
        msg: M,
        reply: oneshot::Sender<Result<Box<dyn std::any::Any + Send>, ActorError>>,
    },
    /// System-level signal (stop, restart, etc.)
    Signal(SystemSignal),
}

/// System signals that bypass normal message processing.
#[derive(Debug, Clone)]
pub enum SystemSignal {
    /// Gracefully stop the actor.
    Stop,
    /// Restart the actor (clear state, re-run pre_start).
    Restart,
    /// Poison pill — stop after processing current message.
    PoisonPill,
}

/// A typed, cloneable handle to an actor. This is the ONLY way to interact
/// with an actor from outside its own message handler.
///
/// ActorRef is cheap to clone and can be sent across threads/tasks.
pub struct ActorRef<M: Message> {
    pub(crate) sender: MailboxSender<M>,
    pub(crate) id: ActorId,
}

/// Unique identifier for an actor instance.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ActorId {
    pub name: String,
    pub path: String,
    pub uid: uuid::Uuid,
}

impl ActorId {
    pub fn new(name: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            path: path.into(),
            uid: uuid::Uuid::now_v7(),
        }
    }
}

impl fmt::Display for ActorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.path, self.uid)
    }
}

impl<M: Message> ActorRef<M> {
    /// Send a message to the actor without waiting for a response.
    /// This is the primary communication pattern (tell / fire-and-forget).
    pub fn tell(&self, msg: M) -> Result<(), ActorError> {
        self.sender.send(Envelope::Tell(msg))
    }

    /// Send a message and wait for a typed response.
    /// Times out after the specified duration.
    pub async fn ask<R: Send + 'static>(
        &self,
        msg: M,
        timeout: Duration,
    ) -> Result<R, ActorError> {
        let (tx, rx) = oneshot::channel();

        self.sender.send(Envelope::Ask { msg, reply: tx })?;

        let result = tokio::time::timeout(timeout, rx)
            .await
            .map_err(|_| ActorError::AskTimeout(timeout))?
            .map_err(|_| ActorError::Stopped)?;

        match result {
            Ok(boxed) => boxed
                .downcast::<R>()
                .map(|b| *b)
                .map_err(|_| ActorError::custom("ask reply type mismatch")),
            Err(e) => Err(e),
        }
    }

    /// Send a system signal to the actor.
    pub fn signal(&self, sig: SystemSignal) -> Result<(), ActorError> {
        self.sender.send(Envelope::Signal(sig))
    }

    /// Stop the actor gracefully.
    pub fn stop(&self) -> Result<(), ActorError> {
        self.signal(SystemSignal::Stop)
    }

    /// Get the actor's unique ID.
    pub fn id(&self) -> &ActorId {
        &self.id
    }
}

impl<M: Message> Clone for ActorRef<M> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            id: self.id.clone(),
        }
    }
}

// Re-export Envelope for use by the mailbox module.
// This avoids circular dependencies.

impl<M: Message> fmt::Debug for ActorRef<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ActorRef({})", self.id)
    }
}
