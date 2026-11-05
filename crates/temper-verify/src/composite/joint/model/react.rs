//! What a record's commit sets off: its reactions, as dispatches, and the
//! module calls its triggers start.

use temper_spec::automaton::TargetResolver;
use temper_spec::predicate::{Arg, Expr, Literal, Name, ParamKind, Truth, VarKind, eval};

use super::super::env::{Blind, Found, ID, JointEnv, resolve};
use super::super::room::Heap;
use super::super::senders::Senders;
use super::super::state::ReactionStep;
use super::super::{Call, Committed, Dispatch, JointState};
use super::commit::add;
use super::copies::base;
use super::read::Reaction;
use super::step::Certainty;
use super::{Delivery, JointModel};
use crate::model::{Fields, ParamValue, Sym};

/// What a reaction carries for a string its target requires not blank but
/// does not hold, whatever nonblank value the source held; a blank one it
/// carries as empty.
const NOT_BLANK: &str = "(not blank)";

/// Who made a move, as what follows it reads.
#[derive(Clone, Copy)]
pub(super) struct Sent<'a> {
    /// The chain it came in; empty when it starts one.
    pub(super) chain: &'a [ReactionStep],
    pub(super) by: u32,
    pub(super) params: &'a Fields<ParamValue>,
    /// Whether it is a request, whose sender chose all its parameters.
    pub(super) request: bool,
}

impl JointModel {
    /// What follows `source.action` committing, `sent` by someone: its
    /// reactions, then the module calls its triggers start (when the model
    /// makes the kernel's moves). Each call answers as its sender, inline or
    /// in the background. Each outcome says what the model knows of it.
    pub(super) fn after_commit(
        &self,
        state: JointState,
        source: &str,
        action: &str,
        sent: Sent<'_>,
    ) -> Vec<(JointState, Certainty)> {
        let Sent { chain, by, .. } = sent;
        let mut state = state;
        self.store_outside_spawns(&mut state, source, action);
        // Outside actions and detached work start a fresh chain.
        let root = [ReactionStep {
            entity_type: base(source).into(),
            action: action.into(),
            generated_for: None,
        }];
        let chain = if chain.is_empty() { &root } else { chain };
        let mut outcomes = self.react(state, source, action, Sent { chain, ..sent });
        let Some(specs) = self
            .kernel
            .calls_on
            .get(pair!(source, action))
            .filter(|_| self.kernel_moves)
        else {
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
                    to: source.into(),
                    module: spec.module.as_str().into(),
                    on_success: spec.on_success.iter().map(Sym::from).collect(),
                    on_failure: spec.on_failure.as_ref().map(Sym::from),
                    by: answer_as,
                    asked: asked_entries()
                        .map(|(field, value)| (field.clone(), *value))
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
        sent: Sent<'_>,
    ) -> Vec<(JointState, Certainty)> {
        let Sent {
            chain, by, params, ..
        } = sent;
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
            let as_whom = self.senders.reaction(
                reaction.principal.as_ref(),
                by,
                matches!(reaction.resolve, Some(TargetResolver::SameId)),
            );
            // Fired or not, it leaves every way as it was, so it splits none.
            if self.lands_as_nothing(reaction, as_whom) {
                continue;
            }
            let mut after = Vec::new();
            // The ways before this reaction are dropped as the next ones are
            // made; [`Self::keep`] counted each.
            let before = outcomes.heap();
            for (state, known) in outcomes {
                if !self.expansion.take() {
                    return Vec::new();
                }
                // Making a record takes one record of its type, once.
                let making = match (&reaction.spawn, &reaction.resolve) {
                    (Some(spawn), _) => Some(self.named_id(spawn, params)),
                    (_, Some(TargetResolver::Create)) => Some(None),
                    (_, Some(TargetResolver::CreateIfMissing { id_field })) => {
                        Some(state.records[source].strings.get(id_field).cloned())
                    }
                    _ => None,
                };
                let (index, reaction) = match making {
                    None => (index, reaction),
                    Some(named) => match self.making_takes(&state, source, index, named.as_ref()) {
                        Some(at) => (at, &self.reactions[at]),
                        None => {
                            if !self.keep(&mut after, state, known) {
                                return Vec::new();
                            }
                            continue;
                        }
                    },
                };
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
                    Some(guard) => eval(
                        guard,
                        &JointEnv {
                            params: Some(params),
                            ..self.env_blind(source, &state, Blind::Related)
                        },
                    ),
                    None => Truth::True,
                };
                // The runtime reads the related records a trigger's guard
                // names after its source commits, one trigger after
                // another, while other moves land. A guard that reads one
                // in scope is read as its own move, on the source as it
                // committed; one that reads none the model holds goes both
                // ways now.
                let unread_guard = match (&reaction.guard, fires) {
                    (Some(guard), Truth::Unknown) => self.committed(guard, source, &state, params),
                    _ => None,
                };
                let chosen = sent.request && self.chosen_guards.contains(&index);
                let decided = fires != Truth::Unknown || unread_guard.is_some() || chosen;
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
                    Some(TargetResolver::Create) => Some(Sym::new(base(&reaction.to))),
                    Some(TargetResolver::CreateIfMissing { id_field }) => {
                        if state.records[source]
                            .strings
                            .get(id_field)
                            .is_some_and(|id| !id.is_empty())
                        {
                            None
                        } else {
                            Some(Sym::new(base(&reaction.to)))
                        }
                    }
                    Some(TargetResolver::SameId) => {
                        chain.last().and_then(|step| step.generated_for)
                    }
                    _ => None,
                };
                let step = ReactionStep {
                    entity_type: base(&reaction.to).into(),
                    action: reaction.target_action.as_str().into(),
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
                let (via, chain) = self.sent_as(index, chain, step);
                let dispatch = Dispatch {
                    to: reaction.to.as_str().into(),
                    action: reaction.target_action.as_str().into(),
                    target_state: None,
                    params: self.args(&state, source, reaction),
                    read: !self.action_reads_related(&reaction.to, &reaction.target_action),
                    chain: chain.into(),
                    via: Some(via),
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
            self.expansion.note_flight_read();
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
    fn committed(
        &self,
        guard: &Expr,
        source: &str,
        state: &JointState,
        params: &Fields<ParamValue>,
    ) -> Option<Committed> {
        let model = &self.models[source];
        let record = &state.records[source];
        let mut env = self.env(source, state);
        env.unheld_unknown = true;
        env.params = Some(params);
        if eval(guard, &env) == Truth::Unknown {
            return None;
        }
        let mut fields = Fields::new();
        let mut carried = Fields::new();
        let mut lists = false;
        let mut reads_status = false;
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
                fields.insert(field, value);
            }
        };
        guard.for_each_name(&mut |name| match name {
            Name::Var(field) => keep(field),
            Name::CrossStatus { id_field, .. } | Name::CrossField { id_field, .. } => {
                keep(id_field);
            }
            Name::Status => reads_status = true,
            Name::Param(param) => {
                if let Some(value) = params.get(param) {
                    carried.insert(param, *value);
                }
            }
        });
        if let Some(id) = record.strings.get(ID) {
            fields.insert(ID, ParamValue::Str(*id));
        }
        (!lists).then(|| Committed {
            status: reads_status.then_some(record.status),
            fields,
            params: carried,
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
    /// a value or requires not empty (one it only requires so as
    /// [`NOT_BLANK`] when it is), the counts kept as sequence numbers, and the
    /// counts its target reads (in an effect, or compared with a counter)
    /// from a counter `source` holds as a value, read from `source` as it
    /// holds them.
    pub(super) fn args(
        &self,
        state: &JointState,
        source: &str,
        reaction: &Reaction,
    ) -> Fields<ParamValue> {
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
                    return Some((param.as_str(), ParamValue::Count(count)));
                }
                if self.reads_count(reaction, param) {
                    // Sent only when the source holds it as a value, not as
                    // a sequence number.
                    let Arg::Var(field) = arg else { return None };
                    if self.order.fields(source).any(|(f, _)| f == field) {
                        return None;
                    }
                    let count = source_state.counters.get(field).copied()?;
                    return Some((param.as_str(), ParamValue::Count(count)));
                }
                let modeled = target.models_string(&reaction.target_action, param);
                if !modeled && !nonempty.contains(param) {
                    return None;
                }
                let value = match arg {
                    Arg::Var(field) => source_state.strings.get(field).cloned(),
                    Arg::Lit(Literal::Str(text)) => Some(text.as_str().into()),
                    Arg::Lit(Literal::Null) => Some("".into()),
                    _ => None,
                }?;
                // A string its target does not hold is read only for being
                // blank: every blank value lands the same, and every other.
                let value = match (modeled, value.is_blank()) {
                    (true, _) => value,
                    (false, true) => "".into(),
                    (false, false) => NOT_BLANK.into(),
                };
                Some((param.as_str(), ParamValue::Str(value)))
            })
            .collect()
    }
}

#[cfg(test)]
#[path = "repeat_test.rs"]
mod tests;
