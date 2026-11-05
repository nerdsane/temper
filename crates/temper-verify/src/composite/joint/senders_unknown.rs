//! The sender of a reaction from a type outside a model's scope: its
//! declared principal, or whoever sent its source's move, either of which
//! may be no sender the model knows. Cedar may answer either way for it, and
//! for a reaction that inherits it.

use temper_authz::{Asker, May};

use super::{Senders, base};

impl Senders {
    /// The number of the sender the model does not know; the one after it
    /// is the same sender in a timer's chain.
    pub(in crate::composite::joint) fn unknown(&self) -> u32 {
        (self.askers.len() * 2) as u32
    }

    /// What Cedar lets `by` send that the policies give no answer for: Maybe
    /// for a sender the model does not know, else nothing.
    pub(super) fn unknown_answer(&self, by: u32) -> May {
        if by >= self.unknown() {
            May::Maybe
        } else {
            May::No
        }
    }

    /// Who `by`, a sender the model holds no asker for, is in words.
    pub(super) fn unknown_name(&self, by: u32) -> String {
        if by >= self.unknown() {
            "the sender of a reaction from outside the scope".to_string()
        } else {
            format!("sender {by}")
        }
    }

    /// Whether Cedar lets the outside sender `by` request `action` on an
    /// `entity` record of any type, in scope or not: the table answers for
    /// the types it was read for, the policies for the rest.
    pub(in crate::composite::joint) fn request_anywhere(
        &self,
        by: u32,
        entity: &str,
        action: &str,
    ) -> May {
        let entity = base(entity);
        let Some(requests) = &self.requests else {
            return May::Yes;
        };
        if requests
            .get(entity)
            .is_some_and(|actions| actions.contains_key(action))
        {
            return self.request_as(by, entity, action);
        }
        match (&self.policies, self.askers.get(by as usize / 2)) {
            (Some(policies), Some(asker)) => {
                policies.may_request(asker, action, entity, &|_| Ok(()))
            }
            _ => May::Maybe,
        }
    }

    /// The sender a reaction declaring `service` runs as when `invoker`
    /// sent its source, in `invoker`'s chain: the model does not know a
    /// service it read no policies for.
    pub(in crate::composite::joint) fn declared_as(&self, service: &Asker, invoker: u32) -> u32 {
        let at = self.askers.iter().position(|a| a == service);
        at.map_or(self.unknown(), |at| at as u32 * 2) + invoker % 2
    }
}
