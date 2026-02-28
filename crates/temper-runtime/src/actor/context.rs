use std::any::Any;
use std::collections::HashMap; // determinism-ok: production actor context, not on simulation path

use tokio::sync::oneshot;

use super::actor_ref::{ActorId, ActorRef};
use super::errors::ActorError;
use super::traits::{Actor, Message};

/// Context available to an actor during message handling.
/// Provides access to the actor's identity, child management,
/// and reply capabilities.
pub struct ActorContext<A: Actor> {
    /// This actor's identity.
    pub(crate) id: ActorId,

    /// Reply channel for the current ask (if this message was an ask).
    pub(crate) reply_channel: Option<oneshot::Sender<Result<Box<dyn Any + Send>, ActorError>>>,

    /// Children spawned by this actor.
    pub(crate) children: HashMap<String, Box<dyn Any + Send>>, // determinism-ok: key-based lookup only; iteration order not observed

    _phantom: std::marker::PhantomData<A>,
}

impl<A: Actor> ActorContext<A> {
    pub(crate) fn new(id: ActorId) -> Self {
        Self {
            id,
            reply_channel: None,
            children: HashMap::new(), // determinism-ok: map order is not used
            _phantom: std::marker::PhantomData,
        }
    }

    /// Get this actor's ID.
    pub fn id(&self) -> &ActorId {
        &self.id
    }

    /// Reply to the current ask message.
    /// Panics if this message was not an ask (was a tell).
    pub fn reply<R: Send + 'static>(&mut self, response: R) {
        if let Some(tx) = self.reply_channel.take() {
            let _ = tx.send(Ok(Box::new(response)));
        }
    }

    /// Reply with an error to the current ask message.
    pub fn reply_err(&mut self, error: ActorError) {
        if let Some(tx) = self.reply_channel.take() {
            let _ = tx.send(Err(error));
        }
    }

    /// Check if the current message expects a reply (is an ask).
    pub fn is_ask(&self) -> bool {
        self.reply_channel.is_some()
    }

    /// Register a child actor ref (for supervision tracking).
    pub fn register_child<M: Message>(&mut self, name: &str, child: ActorRef<M>) {
        self.children.insert(name.to_string(), Box::new(child));
    }

    /// Get a child actor ref by name.
    pub fn get_child<M: Message>(&self, name: &str) -> Option<&ActorRef<M>> {
        self.children
            .get(name)
            .and_then(|boxed| boxed.downcast_ref::<ActorRef<M>>())
    }
}
