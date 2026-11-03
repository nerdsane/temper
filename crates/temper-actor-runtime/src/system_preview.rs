//! A handler's next state, computed without committing anything.

use super::ActorSystem;
use crate::actor::{ActorContext, ActorError, ActorHandle, Message};

impl ActorSystem {
    /// The state `to`'s registered handler would store after handling `msg`
    /// on `state`, with nothing committed.
    ///
    /// The handler runs on a copy of `state` in a context with no mailbox
    /// and no pool: its buffered tells are dropped, and an `ask`, `spawn` or
    /// state write it attempts returns an error, so the preview sends,
    /// creates and stores nothing.
    ///
    /// # Errors
    /// `NotFound` when no handler is registered for `to`'s type; otherwise
    /// the handler's own error, such as `Rejected` for a message it refuses.
    pub async fn preview<M: prost::Message>(
        &self,
        to: &ActorHandle,
        state: &[u8],
        msg: M,
    ) -> Result<Vec<u8>, ActorError> {
        let handler = {
            let handlers = self.handlers.read().unwrap();
            handlers.get(&to.actor_type).cloned()
        }
        .ok_or_else(|| ActorError::NotFound(to.actor_type.clone()))?;
        let message_type = {
            let full = std::any::type_name::<M>();
            full.rsplit("::").next().unwrap_or(full)
        };
        let message = Message {
            id: 0,
            from: None,
            to: to.clone(),
            message_type: message_type.to_string(),
            payload: msg.encode_to_vec(),
            correlation_id: None,
            created_at: chrono::DateTime::<chrono::Utc>::default(),
        };
        let ctx = ActorContext::new(to.clone(), None, None, self.handlers.clone());
        let mut next = state.to_vec();
        handler.handle(&ctx, &mut next, &message).await?;
        Ok(next)
    }
}
