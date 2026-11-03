//! The inline steps and detached work an agent context admits.

use crate::idempotency::IdempotencyKey;

use super::{AgentContext, CallbackBudgetExceeded, InlineChain, MAX_CALLBACK_HOPS};

impl AgentContext {
    /// Admit a callback running `action` on an entity, unless its inline
    /// chain has already taken that step or the lineage has used its hops.
    ///
    /// The callback is a new request, so it gets a follow-up key of its own,
    /// derived from this context's key and the integration that sent it
    /// (`source`). It names the integration's run, not its outcome: a retry
    /// of the parent runs the integration again, which may answer otherwise
    /// or fail where it succeeded, so neither the parameters nor the callback
    /// action are in the key. The run's first outcome answers the retry
    /// ([`IdempotencyKey::FollowUp`]). Sibling callbacks differ by `source`.
    pub(crate) fn for_callback(
        &self,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        source: &str,
    ) -> Result<Self, CallbackBudgetExceeded> {
        let step = self.inline_chain.step(entity_type, entity_id, action);
        let inline_chain = self
            .inline_chain
            .then(step)
            .map_err(CallbackBudgetExceeded::RepeatedStep)?;
        if self.callback_hops >= MAX_CALLBACK_HOPS {
            return Err(CallbackBudgetExceeded::TotalHops);
        }
        let mut next = self.clone();
        next.inline_chain = inline_chain;
        next.callback_hops += 1;
        next.idempotency_key = self.callback_key(source);
        Ok(next)
    }

    /// The key of a callback that `source` sends for this context's move:
    /// derived from this context's key, so every run of the move derives it.
    pub(crate) fn callback_key(&self, source: &str) -> Option<IdempotencyKey> {
        self.idempotency_key.as_ref().map(|key| {
            IdempotencyKey::FollowUp(crate::idempotency::derived_idempotency_key(
                key.as_str(),
                "callback",
                source,
            ))
        })
    }

    /// This context inside `action` on an entity: the step of its inline
    /// chain that the action's own inline work continues.
    pub(crate) fn entering(&self, entity_type: &str, entity_id: &str, action: &str) -> Self {
        let mut next = self.clone();
        let step = self.inline_chain.step(entity_type, entity_id, action);
        next.inline_chain = self.inline_chain.entering(step);
        next
    }

    /// This context after its step made these entities (`(type, id)`), so its
    /// chain knows them as made rather than found.
    pub(crate) fn having_made<'a>(
        mut self,
        made: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Self {
        for (entity_type, entity_id) in made {
            self.inline_chain = self.inline_chain.making(entity_type, entity_id);
        }
        self
    }

    /// Begin a detached task without replenishing its causal callback budget.
    ///
    /// Call only at a runtime-owned spawn boundary: the parent inline chain
    /// ends there, but its callback lineage continues in the detached task.
    /// A detached task is not the answer of a call this context answers, so
    /// it removes no call's deadline.
    pub(crate) fn for_background_task(&self) -> Self {
        let mut next = self.clone();
        next.inline_chain = InlineChain::default();
        next.ends_call = None;
        next
    }
}
