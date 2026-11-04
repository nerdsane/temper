use std::fmt;
use std::marker::PhantomData;
use std::time::Duration;

use tokio::sync::oneshot;

use super::errors::ActorError;
use super::traits::Message;
use crate::mailbox::MailboxSender;

type AskReply = Result<Box<dyn std::any::Any + Send>, ActorError>;

/// An envelope wrapping a message with an optional reply channel.
pub enum Envelope<M: Message> {
    /// Fire-and-forget message.
    Tell(M),
    /// Request-response message with a reply channel.
    Ask {
        msg: M,
        reply: oneshot::Sender<Result<Box<dyn std::any::Any + Send>, ActorError>>,
        /// The asker's span: the actor handles the message inside it, so what
        /// the actor does (storage writes, snapshots) is part of the asker's
        /// trace. The asker waits for the reply, so the span is open meanwhile.
        span: tracing::Span,
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
    /// Human-readable name of the actor.
    pub name: String,
    /// Hierarchical path (e.g., "system/orders/order-1").
    pub path: String,
    /// Unique identifier for this actor incarnation.
    pub uid: uuid::Uuid,
}

impl ActorId {
    /// Create a new actor ID with the given name and path.
    pub fn new(name: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            path: path.into(),
            uid: crate::scheduler::sim_uuid(),
        }
    }
}

impl fmt::Display for ActorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.path, self.uid)
    }
}

impl<M: Message> ActorRef<M> {
    /// A closed mailbox cannot recover; a new actor incarnation is required.
    pub fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }

    /// Send a message to the actor without waiting for a response.
    /// This is the primary communication pattern (tell / fire-and-forget).
    pub fn tell(&self, msg: M) -> Result<(), ActorError> {
        self.sender.send(Envelope::Tell(msg))
    }

    /// Send a message and wait for a typed response.
    /// Times out after the specified duration.
    pub async fn ask<R: Send + 'static>(&self, msg: M, timeout: Duration) -> Result<R, ActorError> {
        self.send_pending(msg)?.wait(timeout).await
    }

    /// Send a message and hold its place in the queue. [`PendingAsk::wait`] can
    /// be called again after a timeout, so a caller that keeps waiting does not
    /// add a second copy behind the first. Dropping the handle gives up the ask:
    /// if the message has not started, it never runs.
    pub fn send_pending<R: Send + 'static>(
        &self,
        msg: M,
    ) -> Result<PendingAsk<'_, M, R>, ActorError> {
        Ok(PendingAsk {
            actor: self,
            reply: self.send_ask(msg)?,
            answer: PhantomData,
        })
    }

    /// Send a message and wait until the actor replies or stops, without a
    /// caller deadline. Use when the sender owns work whose outcome must be
    /// learned even if a store takes longer than a request's timeout.
    pub async fn ask_until_reply<R: Send + 'static>(&self, msg: M) -> Result<R, ActorError> {
        let result = self.send_ask(msg)?.await.map_err(|_| ActorError::Stopped)?;
        Self::decode_reply(result)
    }

    fn send_ask(&self, msg: M) -> Result<oneshot::Receiver<AskReply>, ActorError> {
        let (tx, rx) = oneshot::channel();
        self.sender.send(Envelope::Ask {
            msg,
            reply: tx,
            span: tracing::Span::current(),
        })?;
        Ok(rx)
    }

    fn decode_reply<R: Send + 'static>(result: AskReply) -> Result<R, ActorError> {
        result?
            .downcast::<R>()
            .map(|boxed| *boxed)
            .map_err(|_| ActorError::custom("ask reply type mismatch"))
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

    /// Current in-flight mailbox depth (messages queued but not yet processed).
    /// Exposed for observability; see `runtime_metrics::record_actor_mailbox_depth`.
    pub fn mailbox_depth(&self) -> usize {
        self.sender.depth()
    }

    /// Mailbox utilization in `[0.0, 1.0]`.
    pub fn mailbox_utilization(&self) -> f64 {
        self.sender.utilization()
    }

    /// Mailbox total capacity.
    pub fn mailbox_capacity(&self) -> usize {
        self.sender.capacity()
    }
}

/// An ask that has been sent and not yet answered. See [`ActorRef::send_pending`].
pub struct PendingAsk<'a, M: Message, R> {
    actor: &'a ActorRef<M>,
    reply: oneshot::Receiver<AskReply>,
    answer: PhantomData<R>,
}

impl<M: Message, R: Send + 'static> PendingAsk<'_, M, R> {
    /// Wait up to `timeout` for the answer. After [`ActorError::AskTimeout`] the
    /// ask is still queued and this may be called again; after any other answer
    /// the ask is over and this handle must be dropped.
    pub async fn wait(&mut self, timeout: Duration) -> Result<R, ActorError> {
        match tokio::time::timeout(timeout, &mut self.reply).await {
            Ok(Ok(answer)) => ActorRef::<M>::decode_reply(answer),
            Ok(Err(_)) => Err(ActorError::Stopped),
            Err(_) => {
                tracing::warn!(
                    actor = %self.actor.id,
                    timeout_ms = timeout.as_millis() as u64,
                    mailbox_depth = self.actor.mailbox_depth(),
                    mailbox_capacity = self.actor.mailbox_capacity(),
                    "ask timed out"
                );
                Err(ActorError::AskTimeout(timeout))
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actor_id_display_format() {
        let id = ActorId::new("test", "system/test");
        let display = format!("{}", id);
        assert!(display.starts_with("system/test@"));
        assert!(display.contains('@'));
    }

    #[test]
    fn actor_id_equality() {
        let id1 = ActorId {
            name: "a".to_string(),
            path: "p".to_string(),
            uid: uuid::Uuid::nil(),
        };
        let id2 = ActorId {
            name: "a".to_string(),
            path: "p".to_string(),
            uid: uuid::Uuid::nil(),
        };
        assert_eq!(id1, id2);
    }

    #[test]
    fn actor_id_inequality_on_uid() {
        let id1 = ActorId::new("a", "p");
        let id2 = ActorId::new("a", "p");
        // Different sim_uuid() calls produce different UUIDs
        assert_ne!(id1, id2);
    }
}
