use std::fmt;
use std::marker::PhantomData;
use std::time::Duration;

use tokio::sync::oneshot;

use super::errors::ActorError;
use super::traits::Message;
use crate::mailbox::MailboxSender;

/// An envelope wrapping a message with an optional reply channel.
pub enum Envelope<M: Message> {
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

/// Reply handle for an ask that has already been admitted to an actor mailbox.
///
/// [`crate::system::ActorSystem::spawn_with_first_ask`] returns this handle
/// after synchronously placing the ask in a fresh actor's mailbox. Waiting has
/// no independent wall-clock timeout because the handle is coupled to actor
/// startup: it resolves after the first message is handled, or with
/// [`ActorError::Stopped`] when startup permanently fails and drops the reply
/// channel.
pub struct PendingAsk<R> {
    receiver: oneshot::Receiver<Result<Box<dyn std::any::Any + Send>, ActorError>>,
    response: PhantomData<R>,
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
    /// Send a message to the actor without waiting for a response.
    /// This is the primary communication pattern (tell / fire-and-forget).
    pub fn tell(&self, msg: M) -> Result<(), ActorError> {
        self.sender.send(Envelope::Tell(msg))
    }

    /// Send a message and wait for a typed response.
    /// Times out after the specified duration.
    pub async fn ask<R: Send + 'static>(&self, msg: M, timeout: Duration) -> Result<R, ActorError> {
        let pending = self.enqueue_ask(msg)?;

        tokio::time::timeout(timeout, pending.receive())
            .await
            .map_err(|_| ActorError::AskTimeout(timeout))?
    }

    /// Admit an ask synchronously and return its pending reply handle.
    pub(crate) fn enqueue_ask<R: Send + 'static>(
        &self,
        msg: M,
    ) -> Result<PendingAsk<R>, ActorError> {
        let (tx, rx) = oneshot::channel();

        self.sender.send(Envelope::Ask { msg, reply: tx })?;

        Ok(PendingAsk {
            receiver: rx,
            response: PhantomData,
        })
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

    /// Return whether this actor incarnation can no longer receive messages.
    pub fn is_stopped(&self) -> bool {
        self.sender.is_closed()
    }
}

impl<R: Send + 'static> PendingAsk<R> {
    /// Wait for the already-enqueued actor reply.
    pub async fn receive(self) -> Result<R, ActorError> {
        let result = self.receiver.await.map_err(|_| ActorError::Stopped)?;
        match result {
            Ok(boxed) => boxed
                .downcast::<R>()
                .map(|response| *response)
                .map_err(|_| ActorError::custom("ask reply type mismatch")),
            Err(error) => Err(error),
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
    fn actor_id_new_sets_name_and_path() {
        let id = ActorId::new("order-actor", "system/orders/order-1");
        assert_eq!(id.name, "order-actor");
        assert_eq!(id.path, "system/orders/order-1");
    }

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
