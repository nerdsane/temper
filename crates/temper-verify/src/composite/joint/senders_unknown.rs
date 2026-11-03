//! The sender of a reaction from a type outside a model's scope: its
//! declared principal, or whoever sent its source's move, either of which
//! may be no sender the model knows. Cedar may answer either way for it, and
//! for a reaction that inherits it.

use temper_authz::May;

use super::Senders;

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
}
