//! Who outside sends a move: each kind of outside sender that may, where a
//! reaction runs as its sender, and someone outside of no known kind
//! elsewhere.

use temper_authz::May;

use super::super::senders::OUTSIDE;
use super::JointModel;

impl JointModel {
    /// The outside senders that may send `action` to an `entity` record.
    /// Who sends it matters only to a record with a reaction that runs as
    /// them; elsewhere, and for an action outside code is relied on to send
    /// whoever sends it, someone outside of no known kind stands for all.
    pub(super) fn outside_senders(&self, entity: &str, action: &str) -> Vec<u32> {
        let senders = if self.sender_matters.contains(entity) && !self.relied(entity, action) {
            self.senders.outside()
        } else {
            &[OUTSIDE]
        };
        senders
            .iter()
            .copied()
            .filter(|&by| self.outside_may(by, entity, action) != May::No)
            .collect()
    }

    /// What Cedar lets the outside sender `by` send; an action outside code
    /// is relied on to send is taken as allowed for whoever sends it.
    pub(super) fn outside_may(&self, by: u32, entity: &str, action: &str) -> May {
        if self.relied(entity, action) {
            return May::Yes;
        }
        self.senders.request_as(by, entity, action)
    }

    fn relied(&self, entity: &str, action: &str) -> bool {
        self.relies_on
            .contains(&(entity.to_string(), action.to_string()))
    }

    /// The outside sender `by` in a move's words, when it is of a kind.
    pub(super) fn as_sender(&self, by: u32) -> String {
        if by == OUTSIDE {
            String::new()
        } else {
            format!(" as {}", self.senders.name(by))
        }
    }
}
