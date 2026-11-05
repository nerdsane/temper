//! A record taking an action: a dispatch reaching its actor, which takes
//! or refuses it, the state timer that arms, and what the record does next.

use std::collections::BTreeMap;

use temper_spec::predicate::{Expr, Name, Truth, eval};

use super::super::env::{Blind, JointEnv};
use super::super::state::ReactionStep;
use super::super::{Call, Dispatch, JointState};
use super::JointModel;
use super::react::Sent;
use super::step::Certainty;
use crate::model::semantics::{rule_for, step, successors_budgeted, within_bounds};
use crate::model::{
    ParamValue, ResolvedTransition, StringParam, TemperModelAction, TemperModelState,
};

impl JointModel {
    /// The kernel sends `name` to `entity`'s record with no parameters, as
    /// `by` (a timer, or a module's `on_failure`): it goes through the
    /// dispatch path like any dispatch.
    pub(super) fn fire(
        &self,
        state: JointState,
        entity: &str,
        name: &str,
        text: &str,
        by: u32,
    ) -> Vec<(JointState, String, Certainty)> {
        let dispatch = Dispatch {
            to: entity.to_string(),
            action: name.to_string(),
            target_state: None,
            params: BTreeMap::new(),
            read: !self.action_reads_related(entity, name),
            chain: Vec::new(),
            via: None,
            by,
            unread_guard: None,
        };
        if !dispatch.read {
            let mut next = state;
            add(&mut next, dispatch);
            return vec![(next, format!("{text} sent"), Certainty::Sure)];
        }
        self.deliver(state, &dispatch, text)
    }

    /// The states a call that ended unanswered (`text`) leads to from
    /// `next`, where it is no longer in flight: its `on_failure` action, if
    /// it names one, sent as `by`, whoever made the move that called the
    /// module, in the chain the answer comes in.
    pub(super) fn end_call(
        &self,
        next: JointState,
        call: &Call,
        text: String,
        by: u32,
    ) -> Vec<(JointState, String, Certainty)> {
        match &call.on_failure {
            Some(name) => self.fire(
                next,
                &call.to,
                name,
                &format!("{text}: {}.{name}", call.to),
                by,
            ),
            None => vec![(next, text, Certainty::Sure)],
        }
    }

    /// `dispatch` reaches its record's actor in `state`, which takes or
    /// refuses it; what the record does next follows.
    pub(super) fn deliver(
        &self,
        mut state: JointState,
        dispatch: &Dispatch,
        text: &str,
    ) -> Vec<(JointState, String, Certainty)> {
        let refused = format!("{text} refused by its record");
        let (moved, action, certainty) = match self.arrive(&state, dispatch) {
            Arrival::Refused => {
                self.spawn_refused(&mut state, dispatch);
                return vec![(state, refused, Certainty::Sure)];
            }
            Arrival::BeyondBounds => {
                // The runtime can take this move, but its result is outside
                // the bounded walk. Do not count it as a definite refusal
                // or as a covered move in the liveness graph.
                return vec![(
                    state,
                    format!("{text}: outcome beyond exploration bounds"),
                    Certainty::Unsure,
                )];
            }
            Arrival::Took {
                moved,
                action,
                certainty,
            } => (moved, action, certainty),
        };
        // A take the model cannot decide may be a refusal instead.
        let mut outcomes = Vec::new();
        if certainty != Certainty::Sure {
            let mut stays = state.clone();
            self.spawn_refused(&mut stays, dispatch);
            outcomes.push((stays, refused, certainty));
        }
        let text = format!("{text} arrives -> {}", moved.status);
        self.place(&mut state, &dispatch.to, moved, &action, dispatch.by);
        let sent = Sent {
            chain: &dispatch.chain,
            by: dispatch.by,
            params: &dispatch.params,
            request: false,
        };
        let after = self.after_commit(state, &dispatch.to, &action, sent);
        outcomes.extend(
            after
                .into_iter()
                .map(|(outcome, then)| (outcome, text.clone(), certainty.then(then))),
        );
        outcomes
    }

    /// `entity`'s record takes `action` from `by` now; what it does next
    /// follows. No state when its effects cannot apply.
    pub(super) fn commit(
        &self,
        state: JointState,
        entity: &str,
        action: &TemperModelAction,
        chain: &[ReactionStep],
        by: u32,
    ) -> Vec<(JointState, Certainty)> {
        self.commit_as(state, entity, action, chain, by, false)
    }

    /// [`Self::commit`], for a request `by` made with these parameters.
    pub(super) fn commit_request(
        &self,
        state: JointState,
        entity: &str,
        action: &TemperModelAction,
        by: u32,
    ) -> Vec<(JointState, Certainty)> {
        self.commit_as(state, entity, action, &[], by, true)
    }

    fn commit_as(
        &self,
        state: JointState,
        entity: &str,
        action: &TemperModelAction,
        chain: &[ReactionStep],
        by: u32,
        request: bool,
    ) -> Vec<(JointState, Certainty)> {
        let record = &state.records[entity];
        let Some(t) = rule_for(&self.models[entity], &action.name, &record.status) else {
            return Vec::new();
        };
        let moved = step(t, record, &action.params);
        let mut next = state;
        self.place(&mut next, entity, moved, &action.name, by);
        let sent = Sent {
            chain,
            by,
            params: &action.params,
            request,
        };
        self.after_commit(next, entity, &action.name, sent)
    }

    /// Put `entity`'s record, after it took `action` from `by`, into
    /// `state`. Its state timer is armed by `by` when the record entered a
    /// state with one, or `action` resets it (`arm_state_timeouts_if_needed`),
    /// with what `by` had at stake on the record (`Stake`).
    pub(super) fn place(
        &self,
        state: &mut JointState,
        entity: &str,
        moved: TemperModelState,
        action: &str,
        by: u32,
    ) {
        let entered = state.records[entity].status != moved.status;
        let timeout = self.timeout(entity, &moved.status);
        match timeout {
            Some(timeout) if entered || timeout.reset_on.iter().any(|a| a == action) => {
                // The setter's standing is asked of the record before the move.
                let stake = self.senders.stake(by, entity, &state.records[entity]);
                state
                    .armed
                    .insert(entity.into(), stake.arm(self.armer(entity, by)));
            }
            Some(_) => {}
            None => {
                state.armed.remove(entity);
            }
        }
        if let Some(record) = state.records.get_mut(entity) {
            *record = std::sync::Arc::new(moved);
        }
    }

    /// Who `entity`'s timer fires as when `by` arms it. Its fire starts a
    /// timer's chain, whichever chain armed it.
    pub(super) fn armer(&self, entity: &str, by: u32) -> u32 {
        if self.sender_matters.contains(entity) {
            super::super::senders::Senders::armer(by)
        } else {
            super::super::senders::OUTSIDE
        }
    }

    /// What the target's actor does with `dispatch`: refuses it, or takes a
    /// transition, with what the model knows of that. The actor checks its
    /// own state; the related records were read on the dispatch path.
    pub(super) fn arrive(&self, state: &JointState, dispatch: &Dispatch) -> Arrival {
        let model = &self.models[&dispatch.to];
        let target = &state.records[dispatch.to.as_str()];
        let enabled: Vec<&ResolvedTransition> = self
            .transitions_for(dispatch)
            .filter(|t| {
                self.enabled_carrying(
                    &dispatch.to,
                    t,
                    state,
                    Blind::Related,
                    Some(&dispatch.params),
                )
            })
            .collect();
        let Some(&t) = enabled.first() else {
            return Arrival::Refused;
        };
        // Counts and booleans an effect reads and the dispatch does not
        // carry: the first choice, as the composite check takes. A string
        // parameter it does not carry leaves the field as it is.
        let values = self.values(state);
        let mut params: BTreeMap<String, ParamValue> =
            successors_budgeted(model, t, target, &values, &self.expansion)
                .into_iter()
                .next()
                .map(|(params, _)| params)
                .unwrap_or_default();
        for param in &t.string_params {
            params.remove(&param.name);
        }
        let chose = t
            .params
            .keys()
            .any(|name| !dispatch.params.contains_key(name));
        params.extend(dispatch.params.clone());
        let newer = self.order.holds(&dispatch.to, &t.name, &params, target);
        if !constraints_hold(&t.string_params, target, &params) || newer == Truth::False {
            return Arrival::Refused;
        }
        // A count that must equal one of the record's counters, and a
        // string the record does not track that must not be empty: decided
        // when the dispatch carries it.
        let mut carried = true;
        for (param, field) in self.unchecked.counts(&dispatch.to, &t.name) {
            match (dispatch.params.get(param), target.counters.get(field)) {
                (Some(ParamValue::Count(sent)), Some(held)) if sent != held => {
                    return Arrival::Refused;
                }
                (Some(ParamValue::Count(_)), Some(_)) => {}
                _ => carried = false,
            }
        }
        match self.exceeds(&dispatch.to, &t.name, &dispatch.params, target) {
            Truth::False => return Arrival::Refused,
            Truth::Unknown => carried = false,
            Truth::True => {}
        }
        for param in self.unchecked.nonempty(&dispatch.to, &t.name) {
            match dispatch.params.get(param) {
                Some(ParamValue::Str(sent)) if sent.is_blank() => return Arrival::Refused,
                Some(ParamValue::Str(_)) => {}
                // A reaction sends its args and nothing else, and the record
                // refuses a move with this one absent.
                None if dispatch
                    .via
                    .is_some_and(|via| !self.reactions[via].args.contains_key(param)) =>
                {
                    return Arrival::Refused;
                }
                _ => carried = false,
            }
        }
        let moved = step(t, target, &params);
        if !within_bounds(model, target, &moved) {
            return Arrival::BeyondBounds;
        }
        // Taken or refused, and nothing else, unless another transition
        // might be the one taken, the model chose a value it was not sent, or
        // a count behind its family's newest moves up by an unknown amount.
        // With one transition whose related records are read by the model,
        // the dispatch path let those parts of its guard through. A guard
        // reading only types outside scope skips Read, so it stays unknown.
        let guard = if self.reads_related(t) && self.transitions_for(dispatch).nth(1).is_none() {
            self.actor_holds(&dispatch.to, t, state, &dispatch.params)
        } else {
            let env = JointEnv {
                params: Some(&dispatch.params),
                ..self.env(&dispatch.to, state)
            };
            eval(&t.guard, &env) == Truth::True
        };
        let decided = guard
            && self.unchecked.decides(&dispatch.to, &t.name)
            && carried
            && newer == Truth::True;
        let certainty =
            if enabled.len() > 1 || chose || !self.order.exact(&dispatch.to, &t.effects, state) {
                Certainty::Unsure
            } else {
                Certainty::unless(decided, Certainty::OneOf)
            };
        Arrival::Took {
            moved,
            action: t.name.clone(),
            certainty,
        }
    }

    /// Whether the parts of `t`'s guard that `entity`'s actor checks hold in
    /// `state`: each conjunct that reads the record itself. A conjunct that
    /// reads only related records was read on the dispatch path; on a way
    /// sure moves alone take, that read let the dispatch through.
    fn actor_holds(
        &self,
        entity: &str,
        t: &ResolvedTransition,
        state: &JointState,
        params: &BTreeMap<String, ParamValue>,
    ) -> bool {
        let env = JointEnv {
            params: Some(params),
            ..self.env(entity, state)
        };
        let mut parts = Vec::new();
        conjuncts(&t.guard, &mut parts);
        parts
            .into_iter()
            .all(|part| related_only(part) || eval(part, &env) == Truth::True)
    }

    /// Whether `params` exceed each counter of `record` that `entity`'s
    /// `action` needs a count parameter to exceed, where the model holds the
    /// counter as a value: unknown when `params` lacks one.
    pub(super) fn exceeds(
        &self,
        entity: &str,
        action: &str,
        params: &BTreeMap<String, ParamValue>,
        record: &TemperModelState,
    ) -> Truth {
        let mut truth = Truth::True;
        for (param, field) in self.unchecked.greater(entity, action) {
            match (params.get(param), record.counters.get(field)) {
                (Some(ParamValue::Count(sent)), Some(held)) if sent <= held => {
                    return Truth::False;
                }
                (Some(ParamValue::Count(_)), Some(_)) => {}
                _ => truth = Truth::Unknown,
            }
        }
        truth
    }

    /// The transitions `dispatch` may take on its record.
    pub(super) fn transitions_for<'a>(
        &'a self,
        dispatch: &'a Dispatch,
    ) -> impl Iterator<Item = &'a ResolvedTransition> + 'a {
        self.models[&dispatch.to]
            .transitions
            .iter()
            .filter(move |t| {
                t.name == dispatch.action
                    && dispatch
                        .target_state
                        .as_ref()
                        .is_none_or(|target| t.to_state.as_ref() == Some(target))
            })
    }
}

/// What a record's actor does with a dispatch.
pub(super) enum Arrival {
    /// Its result lies outside the bounded exploration, not a refusal.
    BeyondBounds,
    /// It refuses it: no transition from its state allows it.
    Refused,
    /// It takes a transition.
    Took {
        /// The record after it.
        moved: TemperModelState,
        /// The transition's action.
        action: String,
        /// Sure, or one of taking and refusing, or unsure.
        certainty: Certainty,
    },
}

/// The conjuncts of `guard`, flattened.
fn conjuncts<'a>(guard: &'a Expr, parts: &mut Vec<&'a Expr>) {
    match guard {
        Expr::And(inner) => inner.iter().for_each(|part| conjuncts(part, parts)),
        _ => parts.push(guard),
    }
}

/// Whether `part` reads related records and nothing of its own record but
/// the fields that name them.
fn related_only(part: &Expr) -> bool {
    // Each read of a related record also reads the field naming it, just
    // before: a field read more often than that is read for itself.
    let mut reads: BTreeMap<String, isize> = BTreeMap::new();
    let (mut related, mut status) = (false, false);
    part.for_each_name(&mut |name| match name {
        Name::CrossStatus { id_field, .. } | Name::CrossField { id_field, .. } => {
            related = true;
            *reads.entry(id_field.to_string()).or_default() -= 1;
        }
        Name::Var(field) => *reads.entry(field.to_string()).or_default() += 1,
        Name::Status => status = true,
        Name::Param(_) => {}
    });
    related && !status && reads.values().all(|&count| count <= 0)
}

/// Put `dispatch` in flight, kept in order.
pub(super) fn add(state: &mut JointState, dispatch: Dispatch) {
    let at = state.inflight.partition_point(|d| **d < dispatch);
    state.inflight.insert(at, dispatch.into());
}

/// Whether the string parameters `params` passes satisfy the action's
/// constraints on its own record, which its actor checks as it writes.
fn constraints_hold(
    string_params: &[StringParam],
    target: &TemperModelState,
    params: &BTreeMap<String, ParamValue>,
) -> bool {
    string_params.iter().all(|param| {
        // A string parameter nobody passed is absent, which any constraint
        // on it refuses (`validate_action_params`).
        let Some(ParamValue::Str(value)) = params.get(&param.name) else {
            return !param.nonempty && param.equals.is_empty() && param.not_equals.is_empty();
        };
        let tracked = |var: &String| target.strings.get(var);
        !(param.nonempty && value.is_blank())
            && param.equals.iter().filter_map(tracked).all(|v| v == value)
            && param
                .not_equals
                .iter()
                .filter_map(tracked)
                .all(|v| v != value)
    })
}

#[cfg(test)]
#[path = "commit_test.rs"]
mod tests;
