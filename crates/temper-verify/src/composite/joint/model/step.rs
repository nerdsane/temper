//! The moves of the joint model and the states they lead to: actions sent
//! from outside, state timers, module answers, dispatches read on the
//! dispatch path, and dispatches their records take or refuse.

use std::collections::BTreeMap;

use temper_authz::May;
use temper_spec::predicate::{Truth, eval};

use super::super::env::Blind;
use super::super::senders::{OUTSIDE, Rule, Senders};
use super::super::state::canonical;
use super::super::{Call, Dispatch, JointState};
use super::commit::add;
use super::labels::rename_label;
use super::{Delivery, JointModel, MAX_IN_FLIGHT};
use crate::model::{ModelString, TemperModelAction};

#[path = "guard.rs"]
mod guard;

/// One move of the walk.
#[derive(Debug, Clone)]
pub(in crate::composite::joint) enum Move {
    /// An action whose guard and constraints read only its own record, sent
    /// from outside by a sender: its actor checks them as it writes, so
    /// sending and taking are one step.
    Take(String, TemperModelAction, u32),
    /// An action whose guard reads related records, sent from outside by a
    /// sender: the dispatch path read them and they allowed it; its actor
    /// takes it later.
    Send(String, TemperModelAction, u32),
    /// A reaction from a type outside the scope, as the sender it runs as:
    /// a move from outside the model, whenever its target's guard may let
    /// it ([`from_outside::Outside`]).
    Reaction(String, TemperModelAction, u32),
    /// A record's state timer fires its action, with no parameters, as
    /// whoever armed it.
    Timer(String, String),
    /// A module answers the call a record's trigger started.
    Answer(Call, TemperModelAction),
    /// A module's call fails: its `on_failure` action, if any, is sent.
    Fail(Call),
    /// A module's call is lost: the server restarts while it runs. Its
    /// deadline answers it with its `on_failure` action, if it names one,
    /// as a failed call is answered; otherwise nothing answers it.
    Lost(Call),
    /// The dispatch path reads the related records a reaction needs.
    Read(Dispatch),
    /// The target's actor takes a dispatch, or refuses it.
    Arrive(Dispatch),
}

impl Move {
    /// Whether the system makes this move (a timer, a module, or the
    /// kernel's dispatches) rather than someone outside or a restart.
    pub(in crate::composite::joint) fn by_system(&self) -> bool {
        !matches!(
            self,
            Move::Take(..) | Move::Send(..) | Move::Reaction(..) | Move::Lost(..)
        )
    }

    /// Whether this is a reaction from a type outside the scope.
    pub(in crate::composite::joint) fn is_outside_reaction(&self) -> bool {
        matches!(self, Move::Reaction(..))
    }

    /// The entity and action an outside move sends.
    pub(in crate::composite::joint) fn outside_action(&self) -> Option<(&str, &str)> {
        match self {
            Move::Take(entity, action, _) | Move::Send(entity, action, _) => {
                Some((entity, &action.name))
            }
            _ => None,
        }
    }
}

/// What the model knows about one state a move leads to. The order is
/// from most to least known; a step after a step knows the lesser of the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::composite::joint) enum Certainty {
    /// The move can lead here.
    Sure,
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

/// A state a move leads to.
pub(in crate::composite::joint) struct Next {
    pub(in crate::composite::joint) state: JointState,
    /// The new names of the values the old state held.
    pub(in crate::composite::joint) renamed: BTreeMap<ModelString, ModelString>,
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
        if !self.expansion.take() {
            return (Vec::new(), false);
        }
        use Certainty::{OneOf, Sure, Unsure};
        let mut outcomes: Vec<(JointState, String, Certainty)> = Vec::new();
        match mv {
            Move::Take(entity, action, by) => {
                // An outside move the model cannot decide may also be
                // refused, which leaves the state as it is; one that does not
                // exceed a counter it must is refused.
                let exceeds =
                    self.exceeds(entity, &action.name, &action.params, &state.records[entity]);
                if exceeds == Truth::False {
                    return (Vec::new(), false);
                }
                let known = exceeds == Truth::True
                    && self.truth(entity, action, state) == Truth::True
                    && self.outside_may(*by, entity, &action.name) == May::Yes
                    && self.unchecked.meets(entity, &action.name)
                    && self.exact(entity, &action.name, state);
                let text = format!("{entity}.{action}{}", self.as_sender(*by));
                for (outcome, then) in self.commit(state.clone(), entity, action, &[], *by) {
                    let certainty = Certainty::unless(known, Unsure).then(then);
                    if !self
                        .expansion
                        .push(&mut outcomes, (outcome, text.clone(), certainty))
                    {
                        return (Vec::new(), false);
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
                let text = format!("{entity}.{action} sent{}", self.as_sender(*by));
                if !self.expansion.push(
                    &mut outcomes,
                    (next, text, Certainty::unless(known, Unsure)),
                ) {
                    return (Vec::new(), false);
                };
            }
            Move::Reaction(entity, action, by) => {
                for outcome in self.outside_reaction(state, entity, action, *by) {
                    if !self.expansion.push(&mut outcomes, outcome) {
                        return (Vec::new(), false);
                    }
                }
            }
            Move::Timer(entity, name) => {
                let armed = state.armed.get(entity).copied().unwrap_or(OUTSIDE);
                let by = self.senders.timer(armed);
                let text = if self.sender_matters.contains(entity) {
                    format!("{entity}.{name} (timer, as {})", self.senders.name(by))
                } else {
                    format!("{entity}.{name} (timer)")
                };
                let mut next = state.clone();
                next.armed.remove(entity);
                for outcome in self.fire(next, entity, name, &text, by) {
                    if !self.expansion.push(&mut outcomes, outcome) {
                        return (Vec::new(), false);
                    }
                }
            }
            Move::Answer(call, action) => {
                let mut next = state.clone();
                remove_call(&mut next, call);
                let text = format!(
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
                            format!("{text} sent"),
                            Certainty::unless(known, Unsure),
                        ),
                    ) {
                        return (Vec::new(), false);
                    };
                } else if self.stale(call, &action.name, state)
                    || self.exceeds(
                        &call.to,
                        &action.name,
                        &action.params,
                        &state.records[&call.to],
                    ) == Truth::False
                {
                    // The module answers about the record as the call found
                    // it, and the record has moved on from that, or its count
                    // does not exceed the counter it must.
                    if !self.expansion.push(
                        &mut outcomes,
                        (next, format!("{text} refused by its record"), Sure),
                    ) {
                        return (Vec::new(), false);
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
                            &state.records[&call.to],
                        ) == Truth::True
                        && self.unchecked.meets(&call.to, &action.name)
                        && self.exact(&call.to, &action.name, state);
                    if !known {
                        let refused = format!("{text} refused by its record");
                        if !self
                            .expansion
                            .push(&mut outcomes, (next.clone(), refused, OneOf))
                        {
                            return (Vec::new(), false);
                        };
                    }
                    for (outcome, then) in self.commit(next, &call.to, action, &[], call.by) {
                        let certainty = Certainty::unless(known, OneOf).then(then);
                        if !self
                            .expansion
                            .push(&mut outcomes, (outcome, text.clone(), certainty))
                        {
                            return (Vec::new(), false);
                        };
                    }
                }
            }
            Move::Fail(call) => {
                let mut next = state.clone();
                remove_call(&mut next, call);
                let text = format!("{call} fails");
                for outcome in self.end_call(next, call, text, call.by) {
                    if !self.expansion.push(&mut outcomes, outcome) {
                        return (Vec::new(), false);
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
                let text = format!("{call} is lost: the server restarts");
                let by = self.senders.timer(Senders::armer(call.by));
                for outcome in self.end_call(next, call, text, by) {
                    if !self.expansion.push(&mut outcomes, outcome) {
                        return (Vec::new(), false);
                    }
                }
            }
            Move::Read(dispatch) if dispatch.unread_guard.is_some() => {
                for outcome in self.read_trigger_guard(dispatch, state) {
                    if !self.expansion.push(&mut outcomes, outcome) {
                        return (Vec::new(), false);
                    }
                }
            }
            Move::Read(dispatch) => {
                let mut next = state.clone();
                remove(&mut next, dispatch);
                let allows = self.read_allows(dispatch, state);
                if allows != Truth::True {
                    let refused = format!("{dispatch} refused when read");
                    let certainty = Certainty::unless(allows == Truth::False, OneOf);
                    if !self
                        .expansion
                        .push(&mut outcomes, (next.clone(), refused, certainty))
                    {
                        return (Vec::new(), false);
                    };
                }
                if allows != Truth::False {
                    let mut read = dispatch.clone();
                    read.read = true;
                    add(&mut next, read);
                    let text = format!("{dispatch}: related records read");
                    if !self.expansion.push(
                        &mut outcomes,
                        (next, text, Certainty::unless(allows == Truth::True, OneOf)),
                    ) {
                        return (Vec::new(), false);
                    };
                }
            }
            Move::Arrive(dispatch) => {
                let mut next = state.clone();
                remove(&mut next, dispatch);
                // Cedar decides a reaction as its sender; every other
                // dispatch was decided at its entry point, or is the kernel's.
                let may = if let Some(index) = dispatch.via {
                    let reaction = &self.reactions[index];
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
                    let text = format!(
                        "{dispatch} refused by Cedar for {}",
                        self.senders.name(dispatch.by)
                    );
                    if !self.expansion.push(
                        &mut outcomes,
                        (next.clone(), text, Certainty::unless(may == May::No, OneOf)),
                    ) {
                        return (Vec::new(), false);
                    };
                }
                if may != May::No {
                    let cedar = Certainty::unless(may == May::Yes, OneOf);
                    for outcome in self
                        .deliver(next, dispatch, &dispatch.to_string())
                        .into_iter()
                        .map(|(s, t, then)| (s, t, cedar.then(then)))
                    {
                        if !self.expansion.push(&mut outcomes, outcome) {
                            return (Vec::new(), false);
                        }
                    }
                }
            }
        }
        let early = match mv {
            Move::Read(dispatch) | Move::Arrive(dispatch) => self.overtakes(state, dispatch),
            _ => false,
        };
        let mut held_back = false;
        let mut nexts = Vec::new();
        for (mut outcome, text, certainty) in outcomes {
            let certainty = if early { Unsure } else { certainty };
            let fits = self.delivery == Delivery::BeforeNextRequest || self.fits(&outcome);
            held_back |= !fits;
            if !fits {
                continue;
            }
            self.order.normalize(&mut outcome);
            let renamed = canonical(&mut outcome);
            let next = Next {
                label: rename_label(&text, mv, &renamed),
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

    /// Whether the dispatch path lets `dispatch` through: the related
    /// records its guard reads allow it, for one of the transitions it may
    /// take. Its own record is read later, by its actor. True when a guard
    /// holds outright, false when the related records refuse every one, and
    /// unknown when the model cannot tell.
    pub(super) fn read_allows(&self, dispatch: &Dispatch, state: &JointState) -> Truth {
        let full = self.env(&dispatch.to, state);
        let related = self.env_blind(&dispatch.to, state, Blind::Own);
        let mut refused = true;
        for t in self.transitions_for(dispatch) {
            if eval(&t.guard, &full) == Truth::True {
                return Truth::True;
            }
            refused &= eval(&t.guard, &related) == Truth::False;
        }
        if refused {
            Truth::False
        } else {
            Truth::Unknown
        }
    }

    /// Whether the counts kept as sequence numbers stay exact when `entity`
    /// takes `action` in `state`.
    pub(super) fn exact(&self, entity: &str, action: &str, state: &JointState) -> bool {
        self.models[entity]
            .transitions
            .iter()
            .filter(|t| t.name == action)
            .all(|t| self.order.exact(entity, &t.effects, state))
    }

    /// Whether a field `action` ties a parameter to has changed since `call`
    /// started, so its record refuses the module's answer.
    fn stale(&self, call: &Call, action: &str, state: &JointState) -> bool {
        let strings = &state.records[&call.to].strings;
        self.unchecked
            .equals
            .get(&(call.to.clone(), action.to_string()))
            .is_some_and(|fields| {
                fields
                    .iter()
                    .any(|field| call.asked.get(field) != strings.get(field))
            })
    }

    /// Whether the guard of the transition `action` names holds on `state`,
    /// as far as the model knows.
    fn truth(&self, entity: &str, action: &TemperModelAction, state: &JointState) -> Truth {
        let status = &state.records[entity].status;
        self.models[entity]
            .transitions
            .iter()
            .find(|t| {
                t.name == action.name
                    && t.to_state == action.target_state
                    && (t.from_states.is_empty() || t.from_states.contains(status))
            })
            .map_or(Truth::Unknown, |t| eval(&t.guard, &self.env(entity, state)))
    }
}

/// The dispatch an outside sender, or a module's answer, makes after the
/// dispatch path read the related records and they allowed it.
pub(super) fn sent(entity: &str, action: &TemperModelAction, by: u32) -> Dispatch {
    Dispatch {
        to: entity.to_string(),
        action: action.name.clone(),
        target_state: action.target_state.clone(),
        params: action.params.clone(),
        read: true,
        chain: Vec::new(),
        via: None,
        by,
        unread_guard: None,
    }
}

pub(super) fn remove(state: &mut JointState, dispatch: &Dispatch) {
    if let Some(at) = state.inflight.iter().position(|d| d == dispatch) {
        state.inflight.remove(at);
    }
}

fn remove_call(state: &mut JointState, call: &Call) {
    if let Some(at) = state.calls.iter().position(|c| c == call) {
        state.calls.remove(at);
    }
}
