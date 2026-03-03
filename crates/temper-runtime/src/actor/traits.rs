use std::fmt::Debug;

use super::context::ActorContext;
use super::errors::ActorError;
use crate::supervision::SupervisionStrategy;

/// Marker trait for actor messages.
/// Messages must be Send + 'static to cross async boundaries.
pub trait Message: Send + Debug + 'static {}

/// The core actor trait. Erlang/Akka-minimal — no syntactic sugar.
///
/// Every actor has:
/// - A message type (what it receives)
/// - A state type (what it maintains)
/// - Lifecycle hooks (pre_start, post_stop)
/// - A message handler
/// - A supervision strategy (how child failures are handled)
///
/// Actors are spawned into an ActorSystem and communicate via ActorRef<A::Msg>.
/// All interaction is through asynchronous message passing.
pub trait Actor: Send + 'static {
    /// The type of messages this actor can receive.
    type Msg: Message;

    /// The actor's internal state. Rebuilt on restart.
    type State: Send + 'static;

    /// How this actor's children should be supervised when they fail.
    fn supervision_strategy(&self) -> SupervisionStrategy {
        SupervisionStrategy::default()
    }

    /// Called before the actor starts processing messages.
    /// Returns the initial state. If this fails, the actor won't start.
    fn pre_start(
        &self,
        ctx: &mut ActorContext<Self>,
    ) -> impl std::future::Future<Output = Result<Self::State, ActorError>> + Send
    where
        Self: Sized;

    /// Handle a single message. This is the actor's main logic.
    /// Called sequentially — one message at a time, no concurrent access to state.
    fn handle(
        &self,
        msg: Self::Msg,
        state: &mut Self::State,
        ctx: &mut ActorContext<Self>,
    ) -> impl std::future::Future<Output = Result<(), ActorError>> + Send
    where
        Self: Sized;

    /// Called after the actor stops (graceful shutdown or restart).
    /// Use for cleanup. State is consumed — cannot be used after this.
    fn post_stop(
        &self,
        state: Self::State,
        ctx: &mut ActorContext<Self>,
    ) -> impl std::future::Future<Output = ()> + Send
    where
        Self: Sized;
}
