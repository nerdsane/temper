//! Who outside sends a move: each kind of outside sender that may, where a
//! reaction runs as its sender, and someone outside of no known kind
//! elsewhere.

use std::collections::BTreeSet;
use std::sync::Arc;

use temper_authz::May;

use super::super::senders::OUTSIDE;
use super::JointModel;
use crate::model::TemperModelAction;

impl JointModel {
    /// The outside senders a move sends `action` to an `entity` record as:
    /// of those that may, one for each way what follows can go. Who sends it
    /// matters only to a record with a reaction that runs as them; elsewhere,
    /// and for an action outside code is relied on to send whoever sends it,
    /// someone outside of no known kind stands for all.
    pub(super) fn outside_senders(&self, entity: &str, action: &str) -> Arc<[u32]> {
        if let Some(known) = self.senders.alike(entity, action) {
            return known;
        }
        let senders = if self.sender_matters.contains(entity) && !self.relied(entity, action) {
            self.senders.outside()
        } else {
            &[OUTSIDE]
        };
        let asked = self.asked_after(entity, action);
        let mut ways = BTreeSet::new();
        let kept: Arc<[u32]> = senders
            .iter()
            .copied()
            .filter(|&by| {
                let may = self.outside_may(by, entity, action);
                may != May::No && ways.insert(self.way(by, may, &asked))
            })
            .collect();
        self.senders.note_alike(entity, action, kept.clone());
        kept
    }

    /// What decides how the moves after one sent by `by` go, which Cedar was
    /// asked first as `may`. A sender another stands for answers alike every
    /// question `asked`, in every chain, on every hop. An answer that depends
    /// on the request is decided again from the sender's own facts, so such a
    /// sender stands only for itself.
    fn way(
        &self,
        by: u32,
        may: May,
        asked: &BTreeSet<(&str, &str)>,
    ) -> (May, Vec<May>, Option<u32>) {
        let mut answers = Vec::with_capacity(4 * asked.len());
        let mut itself = None;
        for (to, action) in asked {
            for same_id in [true, false] {
                let on_hop = self.senders.reaction(None, by, same_id);
                for sender in [on_hop, self.senders.timer(on_hop)] {
                    let answer = self.senders.may(sender, to, action);
                    if answer == May::Maybe {
                        itself = Some(by);
                    }
                    answers.push(answer);
                }
            }
        }
        (may, answers, itself)
    }

    /// The (entity, action) questions Cedar is asked as whoever sent
    /// `action` to an `entity` record, on anything that follows it: reactions
    /// that run as their sender, and those of the module answers and timers
    /// it sets off, which run as them too.
    fn asked_after<'a>(&'a self, entity: &'a str, action: &'a str) -> BTreeSet<(&'a str, &'a str)> {
        let mut asked = BTreeSet::new();
        let mut seen = BTreeSet::from([(entity, action)]);
        let mut open = vec![(entity, action)];
        while let Some((entity, action)) = open.pop() {
            let mut follows = |next: (&'a str, &'a str)| {
                if seen.insert(next) {
                    open.push(next);
                }
            };
            for reaction in self
                .reactions
                .iter()
                .filter(|r| r.principal.is_none() && r.from == entity && r.source_action == action)
            {
                let next = (reaction.to.as_str(), reaction.target_action.as_str());
                // Cedar is not asked about a spawned record's first action.
                if reaction.spawn.is_none() {
                    asked.insert(next);
                }
                follows(next);
            }
            for spec in self
                .kernel
                .calls_on
                .get(pair!(entity, action))
                .into_iter()
                .flatten()
            {
                for name in spec.on_success.iter().chain(&spec.on_failure) {
                    follows((entity, name.as_str()));
                }
                if spec.on_success.is_empty() {
                    let model = &self.models[entity];
                    for t in &model.transitions {
                        let internal = self
                            .kernel
                            .internal
                            .get(entity)
                            .is_some_and(|names| names.contains(t.name.as_str()));
                        if internal || self.only_a_module_may(entity, &t.name) {
                            follows((entity, t.name.as_str()));
                        }
                    }
                }
            }
            let Some(timers) = self.kernel.timers.get(entity) else {
                continue;
            };
            let enters: Vec<&str> = self.models[entity]
                .transitions
                .iter()
                .filter(|t| t.name == action)
                .filter_map(|t| t.to_state.as_deref())
                .collect();
            for timer in timers {
                let armed = enters.contains(&timer.state.as_str())
                    || timer.reset_on.iter().any(|name| name == action);
                if armed {
                    follows((entity, timer.action.as_str()));
                }
            }
        }
        asked
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
        self.relies_on.contains(pair!(entity, action))
    }

    /// A request `by` makes in a move's words: `then` after the action, then
    /// who sent it, when it is of a kind.
    pub(super) fn request_label(
        &self,
        entity: &str,
        action: &TemperModelAction,
        by: u32,
        then: &str,
    ) -> String {
        if by == OUTSIDE {
            words!(self, "{entity}.{action}{then}")
        } else {
            let sender = self.senders.name_asking(by, entity, &action.name);
            words!(self, "{entity}.{action}{then} as {sender}")
        }
    }
}
