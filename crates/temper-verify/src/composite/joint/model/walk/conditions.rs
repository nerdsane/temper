//! A way to a state that rests on what Cedar answers.
//!
//! The walk goes both ways on a request Cedar's answer to which depends on who
//! asks, as TLA+ would, on a request's guard on its own record's fields, and on a reaction
//! from a type it doesn't hold. It also goes both ways on a move whose
//! parameter the kernel binds to the caller when only some senders of its kind
//! name one. A verdict on a way through only those says which it rests on: it
//! holds if the runtime can make those moves.
//!
//! Every other way the model cannot be sure of says nothing, because the step
//! may be one the runtime never makes: Cedar's answer on a record's field alone,
//! which some caller can't bring about, a guard on another record the walk
//! doesn't hold, a trigger's guard, a module's answer nothing names, a spawn the
//! model holds too few records for, the kernel refusing such a move for a
//! sender who names no caller (the anonymous caller of a public endpoint).
//! So do two more kinds. One that needs one sender's one request to be allowed
//! and refused needs Cedar to answer two ways. One through a reaction from
//! outside that carries values takes any value the model likes, where the
//! reaction only ever sends what its trigger says.

use std::collections::BTreeSet;

use temper_authz::May;

use super::super::super::state::render;
use super::super::super::{JointState, JointStep};
use super::super::copies::base;
use super::super::{Certainty, JointModel, Move};
use super::Explored;
use crate::model::TemperModelAction;

impl JointModel {
    /// The way the walk found to the state walked `index`th, each step saying
    /// whether it rests on what the model cannot decide; `None` when that way
    /// says nothing. An invariant walks again with a reaction's source in scope,
    /// which says more, so only with `reactions` does one count here.
    pub(in crate::composite::joint) fn way_resting_on_moves(
        &self,
        explored: &Explored,
        mut index: usize,
        reactions: bool,
    ) -> Option<Vec<JointStep>> {
        let mut steps = Vec::new();
        let mut allowed = BTreeSet::new();
        let mut refused = BTreeSet::new();
        loop {
            let state = explored.state(index);
            let Some((from, _)) = explored.nodes[index].parent else {
                steps.push(JointStep {
                    action: None,
                    state: render(&state),
                    unsure: false,
                });
                break;
            };
            let text = explored
                .label(self, index)
                .expect("a state with a parent has a move into it");
            let text = text.as_str();
            let before = explored.state(from as usize);
            let (mv, certainty) = self.step_between(&before, &state, text)?;
            if certainty != Certainty::Sure {
                match &mv {
                    Move::Arrive(dispatch) if certainty == Certainty::Either => {
                        let cedar = self.senders.turns_on_who_asks(
                            dispatch.by,
                            &dispatch.to,
                            &dispatch.action,
                        );
                        if cedar {
                            let answers = if text.contains(" refused by Cedar") {
                                &mut refused
                            } else {
                                &mut allowed
                            };
                            answers.insert(asking(&dispatch.to, &dispatch.action, dispatch.by));
                        } else if self.binds_surely(&dispatch.to, &dispatch.action, dispatch.by)
                            || text.contains(" refused by")
                        {
                            return None;
                        }
                    }
                    Move::Take(entity, action, by)
                        if self.take_rests_on_cedar_or_own_guard(entity, action, &before)
                            && self.request_commits_surely(&before, entity, action, *by) =>
                    {
                        if self.outside_may(*by, entity, &action.name) != May::Yes {
                            if !self.senders.turns_on_who_asks(*by, entity, &action.name) {
                                return None;
                            }
                            allowed.insert(asking(entity, &action.name, *by));
                        }
                    }
                    Move::Reaction(entity, action, by)
                        if reactions
                            && action.params.is_empty()
                            && self.reaction_commits_surely(&before, entity, action, *by) => {}
                    _ => return None,
                }
            }
            steps.push(JointStep {
                action: Some(text.to_string()),
                state: render(&state),
                unsure: certainty != Certainty::Sure,
            });
            index = from as usize;
        }
        if !allowed.is_disjoint(&refused) {
            return None;
        }
        steps.reverse();
        Some(steps)
    }

    /// Whether what a request sets off leaves nothing open but Cedar's answers.
    fn request_commits_surely(
        &self,
        before: &JointState,
        entity: &str,
        action: &TemperModelAction,
        by: u32,
    ) -> bool {
        self.commit_request(before.clone(), entity, action, by)
            .iter()
            .all(|(_, then)| *then <= Certainty::Either)
    }

    /// Whether all a reaction from outside leaves open is whether it comes.
    fn reaction_commits_surely(
        &self,
        before: &JointState,
        entity: &str,
        action: &TemperModelAction,
        by: u32,
    ) -> bool {
        let chain = self.outside_chain(entity, &action.name);
        self.commit(before.clone(), entity, action, &chain, by, None)
            .iter()
            .all(|(_, then)| *then <= Certainty::Either)
    }

    /// The move named `text` from `before` to `after`, and the most the model
    /// knows of it.
    fn step_between(
        &self,
        before: &JointState,
        after: &JointState,
        text: &str,
    ) -> Option<(Move, Certainty)> {
        self.moves(before)
            .into_iter()
            .flat_map(|mv| {
                let found = self
                    .next(before, &mv)
                    .0
                    .into_iter()
                    .filter(|next| next.label == text && next.state == *after)
                    .map(|next| next.certainty)
                    .min();
                found.map(|certainty| (mv, certainty))
            })
            .min_by_key(|(_, certainty)| *certainty)
    }
}

/// One sender's one request.
fn asking(entity: &str, action: &str, by: u32) -> (String, String, u32) {
    // Cedar can't tell a type's records apart, so one sender gets one answer for all of them.
    (base(entity).to_string(), action.to_string(), by)
}
