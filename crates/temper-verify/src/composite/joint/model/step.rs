//! The moves of the joint model and the states they lead to: actions sent
//! from outside, state timers, module answers, dispatches read on the
//! dispatch path, and dispatches their records take or refuse.

use crate::model::Renames;

use temper_authz::May;
use temper_spec::predicate::Truth;

use super::super::senders::{Rule, Senders};
use super::super::state::canonical;
use super::super::{Call, Dispatch, JointState};
use super::commit::add;
use super::labels::rename_label;
use super::{Delivery, JointModel, MAX_IN_FLIGHT};
use crate::model::TemperModelAction;

#[path = "guard.rs"]
mod guard;

pub(in crate::composite::joint) use super::moves::Move;

/// What the model knows about one state a move leads to. The order is
/// from most to least known; a step after a step knows the lesser of the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::composite::joint) enum Certainty {
    /// The move can lead here.
    Sure,
    /// Cedar may answer this request either way and the model cannot say
    /// which. Each is a way the runtime goes if Cedar answers so.
    Either,
    /// The move leads to one of the states it lists with this, and the model
    /// cannot tell which: a guard or constraint it cannot decide, taken and
    /// refused, or Cedar that may refuse.
    OneOf,
    /// The move may lead here, and may also end somewhere it does not list.
    Unsure,
}

impl Certainty {
    /// What is known of a step followed by `then`.
    pub(in crate::composite::joint) fn then(self, then: Certainty) -> Certainty {
        self.max(then)
    }

    /// `Sure` when `known`, else `fallback`.
    pub(in crate::composite::joint) fn unless(known: bool, fallback: Certainty) -> Certainty {
        if known { Certainty::Sure } else { fallback }
    }
}

/// A way a move goes: the state, the move in words, and what the model knows.
pub(in crate::composite::joint) type Outcome = (JointState, String, Certainty);

/// A state a move leads to.
pub(in crate::composite::joint) struct Next {
    pub(in crate::composite::joint) state: JointState,
    /// The new names of the values the old state held.
    pub(in crate::composite::joint) renamed: Renames,
    /// The move, named as in the new state.
    pub(in crate::composite::joint) label: String,
    /// What the model knows of the move leading here.
    pub(in crate::composite::joint) certainty: Certainty,
}

impl JointModel {
    /// The states `mv` leads to from `state`, each with what the model knows
    /// of it, and whether a state was held back because it would put more
    /// than [`MAX_IN_FLIGHT`] dispatches in flight.
    pub(in crate::composite::joint) fn next(
        &self,
        state: &JointState,
        mv: &Move,
    ) -> (Vec<Next>, bool) {
        match self.outcomes(state, mv) {
            Some(outcomes) => self.finished(state, mv, outcomes),
            None => (Vec::new(), false),
        }
    }

    /// What `mv` does to `state`, before what is in flight is looked at as a
    /// whole. It reads only the records, the timers armed, the values the
    /// state holds and its sequence numbers, unless it says it read more with
    /// [`crate::model::semantics::Expansion::note_flight_read`]: a walk
    /// remembers it by those. None when the expansion's room is used up.
    pub(in crate::composite::joint) fn outcomes(
        &self,
        state: &JointState,
        mv: &Move,
    ) -> Option<Vec<Outcome>> {
        if !self.expansion.take() {
            return None;
        }
        use Certainty::{Either, OneOf, Sure, Unsure};
        let mut outcomes: Vec<Outcome> = Vec::new();
        match mv {
            Move::Take(entity, action, by) => {
                // An outside move the model cannot decide may also be
                // refused, which leaves the state as it is; one that does not
                // exceed a counter it must is refused.
                let exceeds = self.exceeds(
                    entity,
                    &action.name,
                    &action.params,
                    &state.records[entity.as_str()],
                );
                if exceeds == Truth::False {
                    return None;
                }
                let known = exceeds == Truth::True
                    && self.truth(entity, action, state) == Truth::True
                    && self.outside_may(*by, entity, &action.name) == May::Yes
                    && self.unchecked.meets(entity, &action.name)
                    && self.exact(entity, &action.name, state);
                let text = self.request_label(entity, action, *by, "");
                for (outcome, then) in self.commit_request(state.clone(), entity, action, *by) {
                    let certainty = Certainty::unless(known, Unsure).then(then);
                    if !self
                        .expansion
                        .push(&mut outcomes, (outcome, text.clone(), certainty))
                    {
                        return None;
                    };
                }
            }
            Move::Send(entity, action, by) => {
                // Its record's guard is read on the dispatch path now, which
                // may refuse it when the model cannot decide the guard, and
                // in its actor later.
                let known = self.outside_may(*by, entity, &action.name) == May::Yes
                    && self.truth(entity, action, state) == Truth::True;
                let mut next = state.clone();
                add(&mut next, sent(entity, action, *by));
                let text = self.request_label(entity, action, *by, " sent");
                if !self.expansion.push(
                    &mut outcomes,
                    (next, text, Certainty::unless(known, Unsure)),
                ) {
                    return None;
                };
            }
            Move::Reaction(entity, action, by) => {
                for outcome in self.outside_reaction(state, entity, action, *by) {
                    if !self.expansion.push(&mut outcomes, outcome) {
                        return None;
                    }
                }
            }
            Move::Timer(entity, name) => {
                for outcome in self.fire_timer(state, entity, name) {
                    if !self.expansion.push(&mut outcomes, outcome) {
                        return None;
                    }
                }
            }
            Move::Answer(call, action) => {
                let mut next = state.clone();
                remove_call(&mut next, call);
                let text = words!(
                    self,
                    "{}.{action} (answer from {}, as {})",
                    call.to,
                    call.module,
                    self.senders.name(call.by)
                );
                if self.action_reads_related(&call.to, &action.name) {
                    // The dispatch path reads its guard now, and may refuse
                    // it when the model cannot decide the guard.
                    let known = self.truth(&call.to, action, state) == Truth::True;
                    add(&mut next, sent(&call.to, action, call.by));
                    if !self.expansion.push(
                        &mut outcomes,
                        (
                            next,
                            words!(self, "{text} sent"),
                            Certainty::unless(known, Unsure),
                        ),
                    ) {
                        return None;
                    };
                } else if self.stale(call, &action.name, state)
                    || self.exceeds(
                        &call.to,
                        &action.name,
                        &action.params,
                        &state.records[call.to.as_str()],
                    ) == Truth::False
                {
                    // The module answers about the record as the call found
                    // it, and the record has moved on from that, or its count
                    // does not exceed the counter it must.
                    if !self.expansion.push(
                        &mut outcomes,
                        (next, words!(self, "{text} refused by its record"), Sure),
                    ) {
                        return None;
                    };
                } else {
                    // The record refuses an answer whose guard or constraints
                    // do not hold, and the call ends there
                    // (`record_generated_callback_refusal`).
                    let known = self.truth(&call.to, action, state) == Truth::True
                        && self.exceeds(
                            &call.to,
                            &action.name,
                            &action.params,
                            &state.records[call.to.as_str()],
                        ) == Truth::True
                        && self.unchecked.meets(&call.to, &action.name)
                        && self.exact(&call.to, &action.name, state);
                    if !known {
                        let refused = words!(self, "{text} refused by its record");
                        if !self
                            .expansion
                            .push(&mut outcomes, (next.clone(), refused, OneOf))
                        {
                            return None;
                        };
                    }
                    for (outcome, then) in self.commit(next, &call.to, action, &[], call.by) {
                        let certainty = Certainty::unless(known, OneOf).then(then);
                        if !self
                            .expansion
                            .push(&mut outcomes, (outcome, text.clone(), certainty))
                        {
                            return None;
                        };
                    }
                }
            }
            Move::Fail(call) => {
                let mut next = state.clone();
                remove_call(&mut next, call);
                let text = words!(self, "{call} fails");
                for outcome in self.end_call(next, call, text, call.by) {
                    if !self.expansion.push(&mut outcomes, outcome) {
                        return None;
                    }
                }
            }
            Move::Lost(call) => {
                // The kernel keeps a call's deadline with the move that
                // started it, so after the restart its `on_failure` answers
                // it as a failure's does (`request_context::calls`). The
                // deadline is a scheduled action: it fires as whoever made
                // that move, starting a timer's chain.
                let mut next = state.clone();
                remove_call(&mut next, call);
                let text = words!(self, "{call} is lost: the server restarts");
                let by = self.senders.timer(Senders::armer(call.by));
                for outcome in self.end_call(next, call, text, by) {
                    if !self.expansion.push(&mut outcomes, outcome) {
                        return None;
                    }
                }
            }
            Move::Silent(call) => {
                let mut next = state.clone();
                remove_call(&mut next, call);
                let text = words!(self, "{call} ends with no answer");
                if !self.expansion.push(&mut outcomes, (next, text, Sure)) {
                    return None;
                }
            }
            Move::Read(dispatch) if dispatch.unread_guard.is_some() => {
                for outcome in self.read_trigger_guard(dispatch, state) {
                    if !self.expansion.push(&mut outcomes, outcome) {
                        return None;
                    }
                }
            }
            Move::Read(dispatch) => {
                let mut next = state.clone();
                remove(&mut next, dispatch);
                let allows = self.read_allows(dispatch, state);
                if allows != Truth::True {
                    let refused = words!(self, "{dispatch} refused when read");
                    let certainty = Certainty::unless(allows == Truth::False, OneOf);
                    let mut stays = next.clone();
                    self.spawn_refused(&mut stays, dispatch);
                    if !self
                        .expansion
                        .push(&mut outcomes, (stays, refused, certainty))
                    {
                        return None;
                    };
                }
                if allows != Truth::False {
                    let mut read = (**dispatch).clone();
                    read.read = true;
                    add(&mut next, read);
                    let text = words!(self, "{dispatch}: related records read");
                    if !self.expansion.push(
                        &mut outcomes,
                        (next, text, Certainty::unless(allows == Truth::True, OneOf)),
                    ) {
                        return None;
                    };
                }
            }
            Move::Arrive(dispatch) => {
                let mut next = state.clone();
                remove(&mut next, dispatch);
                // Cedar decides a reaction as its sender; every other
                // dispatch was decided at its entry point, or is the kernel's,
                // as a spawned record's first action is.
                let via = dispatch.via.map(|index| &self.reactions[index]);
                let may = if let Some(reaction) = via.filter(|r| r.spawn.is_none()) {
                    let rule = Rule {
                        from: &reaction.from,
                        action: &reaction.source_action,
                        trigger: &reaction.trigger,
                    };
                    self.senders
                        .may_in(dispatch.by, &rule, &dispatch.to, &dispatch.action, state)
                } else {
                    May::Yes
                };
                if may != May::Yes {
                    let text = words!(
                        self,
                        "{dispatch} refused by Cedar for {}",
                        self.senders.name(dispatch.by)
                    );
                    if !self.expansion.push(
                        &mut outcomes,
                        (
                            next.clone(),
                            text,
                            Certainty::unless(may == May::No, Either),
                        ),
                    ) {
                        return None;
                    };
                }
                if may != May::No {
                    let cedar = Certainty::unless(may == May::Yes, Either);
                    for outcome in self
                        .deliver(next, dispatch, &words!(self, "{dispatch}"))
                        .into_iter()
                        .map(|(s, t, then)| (s, t, cedar.then(then)))
                    {
                        if !self.expansion.push(&mut outcomes, outcome) {
                            return None;
                        }
                    }
                }
            }
        }
        Some(outcomes)
    }

    /// What `outcomes`, the ways `mv` goes from `state`, come to: each that
    /// fits what the walk holds in flight, in its one form.
    pub(in crate::composite::joint) fn finished(
        &self,
        state: &JointState,
        mv: &Move,
        outcomes: Vec<Outcome>,
    ) -> (Vec<Next>, bool) {
        use Certainty::Unsure;
        let unsure = self.unsure(state, mv);
        let mut held_back = false;
        let mut nexts = Vec::new();
        for (mut outcome, text, certainty) in outcomes {
            let certainty = if unsure { Unsure } else { certainty };
            let fits = self.delivery == Delivery::BeforeNextRequest || self.fits(&outcome);
            held_back |= !fits;
            if !fits {
                continue;
            }
            self.order.normalize(&mut outcome);
            self.in_one_order(&mut outcome);
            let renamed = canonical(&mut outcome);
            let next = Next {
                label: if self.expansion.words() {
                    rename_label(&text, mv, &renamed)
                } else {
                    String::new()
                },
                state: outcome,
                renamed,
                certainty,
            };
            if !self.expansion.push(&mut nexts, next) {
                return (Vec::new(), held_back);
            }
        }
        (nexts, held_back)
    }

    /// Whether nothing is known of where `mv` leads from `state`, whatever
    /// its ways say.
    pub(in crate::composite::joint) fn unsure(&self, state: &JointState, mv: &Move) -> bool {
        match mv {
            Move::Read(dispatch) | Move::Arrive(dispatch) => self.overtakes(state, dispatch),
            Move::Answer(call, action) => self.only_a_module_may(&call.to, &action.name),
            _ => false,
        }
    }
}

/// The dispatch an outside sender, or a module's answer, makes after the
/// dispatch path read the related records and they allowed it.
pub(super) fn sent(entity: &str, action: &TemperModelAction, by: u32) -> Dispatch {
    Dispatch {
        to: entity.into(),
        action: action.name,
        target_state: action.target_state,
        params: action.params.clone(),
        read: true,
        chain: std::sync::Arc::new([]),
        via: None,
        by,
        unread_guard: None,
    }
}

pub(super) fn remove(state: &mut JointState, dispatch: &Dispatch) {
    if let Some(at) = state.inflight.iter().position(|d| **d == *dispatch) {
        state.inflight.remove(at);
    }
}

fn remove_call(state: &mut JointState, call: &Call) {
    if let Some(at) = state.calls.iter().position(|c| **c == *call) {
        state.calls.remove(at);
    }
}
