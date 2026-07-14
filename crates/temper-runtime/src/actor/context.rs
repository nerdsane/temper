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

    /// Whether the actor should stop after the current message finishes.
    pub(crate) stop_requested: bool,

    _phantom: std::marker::PhantomData<A>,
}

impl<A: Actor> ActorContext<A> {
    pub(crate) fn new(id: ActorId) -> Self {
        Self {
            id,
            reply_channel: None,
            children: HashMap::new(), // determinism-ok: map order is not used
            stop_requested: false,
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

    /// Reply to the current ask and stop after the handler returns.
    ///
    /// The stop is requested only if the reply is delivered. This lets callers
    /// retry a timed-out operation without leaving the actor stopped behind a
    /// stale registry reference.
    pub fn reply_and_stop<R: Send + 'static>(&mut self, response: R) {
        if let Some(tx) = self.reply_channel.take() {
            self.stop_requested = tx.send(Ok(Box::new(response))).is_ok();
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervision::SupervisionStrategy;

    struct TestActor;

    #[derive(Debug)]
    struct TestMsg;

    impl Message for TestMsg {}

    impl Actor for TestActor {
        type Msg = TestMsg;
        type State = ();

        async fn pre_start(
            &self,
            _ctx: &mut ActorContext<Self>,
        ) -> Result<Self::State, ActorError> {
            Ok(())
        }

        async fn handle(
            &self,
            _msg: TestMsg,
            _state: &mut Self::State,
            _ctx: &mut ActorContext<Self>,
        ) -> Result<(), ActorError> {
            Ok(())
        }

        fn supervision_strategy(&self) -> SupervisionStrategy {
            SupervisionStrategy::Stop
        }

        async fn post_stop(&self, _state: Self::State, _ctx: &mut ActorContext<Self>) {}
    }

    #[test]
    fn reply_and_stop_requires_a_live_ask_receiver() {
        let id = ActorId::new("test", "test/reply-and-stop");
        let mut ctx = ActorContext::<TestActor>::new(id);
        let (reply, receiver) = oneshot::channel();
        ctx.reply_channel = Some(reply);
        drop(receiver);

        ctx.reply_and_stop("done");

        assert!(!ctx.stop_requested);
    }

    #[test]
    fn reply_and_stop_requests_stop_after_delivery() {
        let id = ActorId::new("test", "test/reply-and-stop");
        let mut ctx = ActorContext::<TestActor>::new(id);
        let (reply, mut receiver) = oneshot::channel();
        ctx.reply_channel = Some(reply);

        ctx.reply_and_stop("done");

        assert!(ctx.stop_requested);
        let response = receiver
            .try_recv()
            .expect("reply should be delivered")
            .expect("reply should succeed");
        assert_eq!(*response.downcast::<&str>().expect("reply type"), "done");
    }
}
