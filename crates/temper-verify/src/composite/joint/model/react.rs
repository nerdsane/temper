//! What a record's commit sets off: its reactions, as dispatches, and the
//! module calls its triggers start.

use std::collections::BTreeMap;

use temper_spec::automaton::TargetResolver;
use temper_spec::predicate::{Arg, Expr, Literal, Name, ParamKind, Truth, VarKind, eval};

use super::super::env::{Blind, Found, ID, resolve};
use super::super::room::Heap;
use super::super::senders::Senders;
use super::super::state::ReactionStep;
use super::super::{Call, Committed, Dispatch, JointState};
use super::commit::add;
use super::read::Reaction;
use super::step::Certainty;
use super::{Delivery, JointModel};
use crate::model::ParamValue;

impl JointModel {
    /// What follows `source.action` committing, sent by `by`: its
    /// reactions, then the module calls its triggers start (when the model
    /// makes the kernel's moves). Each call answers as `by`, inline or in the
    /// background. Each outcome says what the model knows of it.
    pub(super) fn after_commit(
        &self,
        state: JointState,
        source: &str,
        action: &str,
        chain: &[ReactionStep],
        by: u32,
        params: &BTreeMap<String, ParamValue>,
    ) -> Vec<(JointState, Certainty)> {
        let mut state = state;
        self.store_outside_spawns(&mut state, source, action);
        // Outside actions and detached work start a fresh chain.
        let root = [ReactionStep {
            entity_type: source.to_string(),
            action: action.to_string(),
            generated_for: None,
        }];
        let chain = if chain.is_empty() { &root } else { chain };
        let mut outcomes = self.react(state, source, action, chain, by, params);
        let key = (source.to_string(), action.to_string());
        let Some(specs) = self.kernel.calls_on.get(&key).filter(|_| self.kernel_moves) else {
            return outcomes;
        };
        // Who answers reads the same to every sender when no reaction of
        // `source` inherits its sender, so one stands for all of them.
        let answer_as = if self.sender_matters.contains(source) {
            by
        } else {
            Senders::anyone(by)
        };
        let internal = self.kernel.internal.get(source);
        for (outcome, _) in &mut outcomes {
            if !self.expansion.take() {
                return Vec::new();
            }
            for spec in specs {
                let answers = |action: &str| {
                    if spec.on_success.is_empty() {
                        internal.is_some_and(|names| names.contains(action))
                    } else {
                        spec.on_success.iter().any(|name| name == action)
                    }
                };
                let strings = &outcome.records[source].strings;
                let asked_entries = || {
                    self.unchecked
                        .asked(source, answers)
                        .filter_map(|field| strings.get(field).map(|value| (field, value)))
                };
                let count = asked_entries().count();
                let call_heap =
                    crate::model::heap::block(2 * size_of::<usize>() + size_of::<Call>())
                        + crate::model::heap::block(source.len())
                        + spec.module.heap()
                        + spec.on_success.heap()
                        + spec.on_failure.heap()
                        + crate::model::heap::tree_bytes::<String, crate::model::ModelString>(
                            count,
                        )
                        + asked_entries()
                            .map(|(k, v)| k.heap() + v.heap())
                            .sum::<usize>();
                if !self.expansion.hold(call_heap) || !self.expansion.reserve(&mut outcome.calls) {
                    return Vec::new();
                }
                let call = Call {
                    to: source.to_string(),
                    module: spec.module.clone(),
                    on_success: spec.on_success.clone(),
                    on_failure: spec.on_failure.clone(),
                    by: answer_as,
                    asked: asked_entries()
                        .map(|(field, value)| (field.clone(), value.clone()))
                        .collect(),
                };
                let at = outcome.calls.partition_point(|c| **c < call);
                outcome.calls.insert(at, call.into());
            }
        }
        outcomes
    }

    /// Send the reactions of `source.action` between types in scope, in
    /// declaration order, as the runtime does after the source commits:
    /// each is a dispatch carrying its `args` as the source holds them now.
    /// A reaction fires when its guard holds on the source; one whose guard
    /// the model cannot decide (related records are read later) goes one of
    /// the two ways. A reaction whose target reference finds no record goes
    /// nowhere, and one whose reference the model cannot resolve goes one of
    /// those two ways too.
    fn react(
        &self,
        state: JointState,
        source: &str,
        action: &str,
        chain: &[ReactionStep],
        by: u32,
        params: &BTreeMap<String, ParamValue>,
    ) -> Vec<(JointState, Certainty)> {
        let mut outcomes = Vec::new();
        if !self.keep(&mut outcomes, state, Certainty::Sure) {
            return outcomes;
        }
        for (index, reaction) in self
            .reactions
            .iter()
            .enumerate()
            .filter(|(_, r)| r.from == source && r.source_action == action)
        {
            let mut after = Vec::new();
            // The ways before this reaction are dropped as the next ones are
            // made; [`Self::keep`] counted each.
            let before = outcomes.heap();
            for (state, known) in outcomes {
                if !self.expansion.take() {
                    return Vec::new();
                }
                if let Some(spawn) = &reaction.spawn {
                    let made = (index, reaction, spawn);
                    for (next, then) in self.spawn(state, source, made, chain, by, params) {
                        if !self.keep(&mut after, next, known.then(then)) {
                            return Vec::new();
                        }
                    }
                    continue;
                }
                let fires = match &reaction.guard {
                    Some(guard) => eval(guard, &self.env_blind(source, &state, Blind::Related)),
                    None => Truth::True,
                };
                // The runtime reads the related records a trigger's guard
                // names after its source commits, one trigger after
                // another, while other moves land. A guard that reads one
                // in scope is read as its own move, on the source as it
                // committed; one that reads none the model holds goes both
                // ways now.
                let unread_guard = match (&reaction.guard, fires) {
                    (Some(guard), Truth::Unknown) => self.committed(guard, source, &state),
                    _ => None,
                };
                let decided = fires != Truth::Unknown || unread_guard.is_some();
                let known = known.then(Certainty::unless(decided, Certainty::OneOf));
                if fires != Truth::True
                    && unread_guard.is_none()
                    && !self.keep(&mut after, state.clone(), known)
                {
                    return Vec::new();
                }
                if fires == Truth::False {
                    continue;
                }
                let reaches = match &reaction.resolve {
                    Some(TargetResolver::SameId) => {
                        resolve(&self.models, self.records, &state, source, ID, &reaction.to)
                    }
                    Some(TargetResolver::Field { id_field }) => resolve(
                        &self.models,
                        self.records,
                        &state,
                        source,
                        id_field,
                        &reaction.to,
                    ),
                    _ => Found::Unknown,
                };
                if reaches == Found::Missing {
                    if !self.keep(&mut after, state, known) {
                        return Vec::new();
                    }
                    continue;
                }
                let known = known.then(Certainty::unless(reaches == Found::Yes, Certainty::OneOf));
                if reaches == Found::Unknown && !self.keep(&mut after, state.clone(), known) {
                    return Vec::new();
                }
                let generated_for = match &reaction.resolve {
                    Some(TargetResolver::Create) => Some(reaction.to.clone()),
                    Some(TargetResolver::CreateIfMissing { id_field }) => {
                        if state.records[source]
                            .strings
                            .get(id_field)
                            .is_some_and(|id| !id.is_empty())
                        {
                            None
                        } else {
                            Some(reaction.to.clone())
                        }
                    }
                    Some(TargetResolver::SameId) => {
                        chain.last().and_then(|step| step.generated_for.clone())
                    }
                    _ => None,
                };
                let step = ReactionStep {
                    entity_type: reaction.to.clone(),
                    action: reaction.target_action.clone(),
                    generated_for,
                };
                // The runtime stops a step its chain already holds. A chain
                // that came from outside the scope may hold one the model
                // cannot see: then it is stopped or sent.
                let repeats = self.repeats(chain, &step);
                if repeats == Truth::True {
                    if !self.keep(&mut after, state, known) {
                        return Vec::new();
                    }
                    continue;
                }
                let known =
                    known.then(Certainty::unless(repeats == Truth::False, Certainty::OneOf));
                if repeats == Truth::Unknown && !self.keep(&mut after, state.clone(), known) {
                    return Vec::new();
                }
                let mut next_chain = chain.to_vec();
                next_chain.push(step);
                let dispatch = Dispatch {
                    to: reaction.to.clone(),
                    action: reaction.target_action.clone(),
                    target_state: None,
                    params: self.args(&state, source, reaction),
                    read: !self.action_reads_related(&reaction.to, &reaction.target_action),
                    chain: next_chain,
                    via: Some(index),
                    unread_guard,
                    by: self.senders.reaction(
                        reaction.principal.as_ref(),
                        by,
                        matches!(reaction.resolve, Some(TargetResolver::SameId)),
                    ),
                };
                let mut next = state;
                add(&mut next, dispatch);
                if !self.keep(&mut after, next, known) {
                    return Vec::new();
                }
            }
            self.expansion.give(before);
            outcomes = self.within_flight(after);
        }
        // Keep the states charged through module calls and next-state packing.
        outcomes
    }

    /// `ways` without those holding more in flight than the walk keeps
    /// ([`Self::fits`]), but one: a later reaction or call only adds to
    /// them, so the walk would hold each back, and the one kept says that it
    /// did. Without it, `k` reactions whose guards the
    /// model cannot decide make `2^k` ways, nearly all held back.
    fn within_flight(
        &self,
        mut ways: Vec<(JointState, Certainty)>,
    ) -> Vec<(JointState, Certainty)> {
        if self.delivery != Delivery::Any {
            return ways;
        }
        let before = ways.heap();
        let mut over = false;
        ways.retain(|(state, _)| {
            if self.fits(state) {
                return true;
            }
            !std::mem::replace(&mut over, true)
        });
        self.expansion.give(before.saturating_sub(ways.heap()));
        ways
    }

    /// What `guard`, a trigger's guard on `source`, reads of `source` as it
    /// holds it in `state`, when the related records in scope decide it: the
    /// guard is then read later, on those records as they are then. `None`
    /// when they do not (it reads a record the model does not hold, or a
    /// field it does not track, and would read as unknown then too), or the
    /// guard reads a list of its source.
    fn committed(&self, guard: &Expr, source: &str, state: &JointState) -> Option<Committed> {
        let model = &self.models[source];
        let record = &state.records[source];
        let mut env = self.env(source, state);
        env.unheld_unknown = true;
        if eval(guard, &env) == Truth::Unknown {
            return None;
        }
        let mut fields = BTreeMap::new();
        let mut lists = false;
        let mut keep = |field: &str| {
            let value = if matches!(field, "Id" | "id") {
                record.strings.get(ID).cloned().map(ParamValue::Str)
            } else {
                match model.var_kinds.get(field) {
                    Some(VarKind::Counter) => Some(ParamValue::Count(
                        record.counters.get(field).copied().unwrap_or(0),
                    )),
                    Some(VarKind::Bool) => Some(ParamValue::Bool(
                        record.booleans.get(field).copied().unwrap_or(false),
                    )),
                    Some(VarKind::Str) => record.strings.get(field).cloned().map(ParamValue::Str),
                    Some(VarKind::List) => {
                        lists = true;
                        None
                    }
                    Some(VarKind::Num) | None => None,
                }
            };
            if let Some(value) = value {
                let field = if field == "id" { ID } else { field };
                fields.insert(field.to_string(), value);
            }
        };
        guard.for_each_name(&mut |name| match name {
            Name::Var(field) => keep(field),
            Name::CrossStatus { id_field, .. } | Name::CrossField { id_field, .. } => {
                keep(id_field);
            }
            Name::Status | Name::Param(_) => {}
        });
        if let Some(id) = record.strings.get(ID) {
            fields.insert(ID.to_string(), ParamValue::Str(id.clone()));
        }
        (!lists).then(|| Committed {
            status: record.status.clone(),
            fields,
        })
    }

    /// Whether `reaction`'s target action reads its count parameter
    /// `param`: an effect reads it, or a constraint compares it with a
    /// counter.
    fn reads_count(&self, reaction: &Reaction, param: &str) -> bool {
        let (to, action) = (&reaction.to, &reaction.target_action);
        self.unchecked
            .counts(to, action)
            .iter()
            .any(|(counted, _)| counted == param)
            || self.models[to]
                .transitions
                .iter()
                .any(|t| &t.name == action && matches!(t.params.get(param), Some(ParamKind::Count)))
    }

    /// The string parameters `reaction` passes that its target action gives
    /// a value or requires not empty, the counts kept as sequence numbers, and the
    /// counts its target reads (in an effect, or compared with a counter)
    /// from a counter `source` holds as a value, read from `source` as it
    /// holds them.
    pub(super) fn args(
        &self,
        state: &JointState,
        source: &str,
        reaction: &Reaction,
    ) -> BTreeMap<String, ParamValue> {
        let source_state = &state.records[source];
        let target = &self.models[&reaction.to];
        let nonempty = self
            .unchecked
            .nonempty(&reaction.to, &reaction.target_action);
        reaction
            .args
            .iter()
            .filter_map(|(param, arg)| {
                if self
                    .order
                    .carries(&reaction.to, &reaction.target_action, param)
                {
                    let Arg::Var(field) = arg else { return None };
                    let count = source_state.counters.get(field).copied()?;
                    return Some((param.clone(), ParamValue::Count(count)));
                }
                if self.reads_count(reaction, param) {
                    // Sent only when the source holds it as a value, not as
                    // a sequence number.
                    let Arg::Var(field) = arg else { return None };
                    if self.order.fields(source).any(|(f, _)| f == field) {
                        return None;
                    }
                    let count = source_state.counters.get(field).copied()?;
                    return Some((param.clone(), ParamValue::Count(count)));
                }
                if !target.models_string(&reaction.target_action, param)
                    && !nonempty.contains(param)
                {
                    return None;
                }
                let value = match arg {
                    Arg::Var(field) => source_state.strings.get(field).cloned(),
                    Arg::Lit(Literal::Str(text)) => Some(text.clone().into()),
                    Arg::Lit(Literal::Null) => Some("".into()),
                    _ => None,
                }?;
                Some((param.clone(), ParamValue::Str(value)))
            })
            .collect()
    }
}

#[cfg(test)]
#[path = "repeat_test.rs"]
mod tests;
