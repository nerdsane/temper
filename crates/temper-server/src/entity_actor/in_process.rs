//! In-process EntityActor, reached through [`EntityRuntime`].
//!
//! This is a newtype over [`ActorRef<EntityMsg>`]. EntityActor itself stays
//! in this crate. See ADR-0167.

use std::future::Future;
use std::time::Duration;

use temper_runtime::actor::{ActorError, ActorRef};
use temper_runtime::plug::{EntityRuntime, RuntimeRequest};

use super::types::{EntityMsg, EntityResponse};

/// In-process EntityActor mailbox, reached through [`EntityRuntime`].
#[derive(Clone)]
pub struct InProcessEntityRuntime {
    actor: ActorRef<EntityMsg>,
}

impl InProcessEntityRuntime {
    /// Wrap an existing EntityActor mailbox.
    pub fn new(actor: ActorRef<EntityMsg>) -> Self {
        Self { actor }
    }

    /// The underlying mailbox. Used for stop/passivate, not for dispatch.
    pub fn actor_ref(&self) -> &ActorRef<EntityMsg> {
        &self.actor
    }
}

impl From<ActorRef<EntityMsg>> for InProcessEntityRuntime {
    fn from(actor: ActorRef<EntityMsg>) -> Self {
        Self::new(actor)
    }
}

impl EntityRuntime for InProcessEntityRuntime {
    type Response = EntityResponse;
    type Error = ActorError;

    fn execute(
        &self,
        request: RuntimeRequest,
        timeout: Duration,
    ) -> impl Future<Output = Result<EntityResponse, ActorError>> + Send {
        let actor = self.actor.clone();
        async move { actor.ask(EntityMsg::from(&request), timeout).await }
    }
}
