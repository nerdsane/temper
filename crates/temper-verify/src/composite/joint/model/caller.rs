//! What the kernel writes into a parameter it binds (`source`) when it makes
//! a move itself: a reaction, a timer, or a module's answer or failure.

use std::collections::BTreeSet;

use super::super::Dispatch;
use super::super::state::ReactionStep;
use super::JointModel;
use super::commit::constraints_hold;
use crate::model::semantics::successors_budgeted;
use crate::model::{
    Fields, ModelString, ParamValue, ResolvedTransition, StringParam, TemperModelState, source_key,
};
use temper_authz::May;

/// Who made a move, as what follows it reads.
#[derive(Clone, Copy)]
pub(super) struct Sent<'a> {
    /// The chain it came in; empty when it starts one.
    pub(super) chain: &'a [ReactionStep],
    pub(super) by: u32,
    pub(super) params: &'a Fields<ParamValue>,
    /// Whether it is a request, whose sender chose all its parameters.
    pub(super) request: bool,
    /// What its chain has bound, with its own move's ([`Dispatch::caller`]).
    pub(super) caller: Option<&'a Fields<ParamValue>>,
}

/// The values the kernel may bind into a move's tracked parameters.
pub(super) struct Callers {
    /// One set per value it may write; one empty set when it binds none the
    /// model tracks; none when the walk cannot make one within its bounds.
    pub(super) choices: Vec<Fields<ParamValue>>,
    /// Whether the choices are the kernel's to the last: false when a value
    /// the chain bound before is not known, or a constraint on a value the
    /// model chose may refuse the caller the runtime has.
    pub(super) sure: bool,
}

impl JointModel {
    /// The values `t`'s tracked bound parameters may take when `dispatch`
    /// arrives at `target`. `Err` when the kernel refuses the move: a
    /// reaction carries a bound parameter in its args, which no one may
    /// supply, or the sender names no caller (a reaction sent as its own
    /// principal is a service). Otherwise the kernel binds them from whoever
    /// set the chain off: the values the chain bound before, else any a
    /// sender could hold, never empty for the caller's id.
    pub(super) fn caller_ids(
        &self,
        dispatch: &Dispatch,
        t: &ResolvedTransition,
        target: &TemperModelState,
        values: &BTreeSet<ModelString>,
    ) -> Result<Callers, ()> {
        let none = || Callers {
            choices: vec![Fields::new()],
            sure: true,
        };
        if t.bound.is_empty() {
            return Ok(none());
        }
        // A request's or a module's parameters are the model's choice: one
        // that leaves the bound ones out is the one the runtime takes.
        let reaction = dispatch.via.map(|via| &self.reactions[via]);
        let as_service = reaction.is_some_and(|reaction| reaction.principal.is_some());
        let sends = reaction.is_some_and(|reaction| {
            t.bound
                .iter()
                .any(|name| reaction.args.contains_key(name.as_str()))
        });
        if as_service || sends || self.senders.names_a_caller(dispatch.by) == May::No {
            return Err(());
        }
        let tracked: Vec<&StringParam> = t
            .string_params
            .iter()
            .filter(|param| param.source.is_some())
            .collect();
        if tracked.is_empty() {
            return Ok(none());
        }
        let bound_before = |param: &StringParam| {
            let caller = dispatch.caller.as_ref()?;
            caller.get(&source_key(param.source?)).copied()
        };
        let free: Vec<&StringParam> = tracked
            .iter()
            .copied()
            .filter(|param| bound_before(param).is_none())
            .collect();
        let model = &self.models[dispatch.to.as_str()];
        let mut choices: BTreeSet<Fields<ParamValue>> = BTreeSet::new();
        if !free.is_empty() {
            for (params, _) in successors_budgeted(model, t, target, values, &self.expansion) {
                let choice = free
                    .iter()
                    .filter_map(|param| params.get(&param.name).map(|value| (param.name, *value)));
                choices.insert(choice.collect());
            }
        } else {
            choices.insert(Fields::new());
        }
        let before: Vec<_> = tracked
            .iter()
            .filter_map(|param| Some((param.name, bound_before(param)?)))
            .collect();
        let choices = choices
            .into_iter()
            .map(|mut choice| {
                choice.extend(before.iter().copied());
                choice
            })
            .collect();
        // The model's choices meet the constraints already; the runtime's
        // caller may not.
        let constrained = |param: &&StringParam| {
            param.nonempty || !param.equals.is_empty() || !param.not_equals.is_empty()
        };
        let sure = dispatch.caller.is_some() && !free.iter().any(constrained);
        Ok(Callers { choices, sure })
    }

    /// What `t` binds when it moves with `params`, by source.
    pub(super) fn bound_by(
        t: &ResolvedTransition,
        params: &Fields<ParamValue>,
    ) -> Fields<ParamValue> {
        t.string_params
            .iter()
            .filter_map(|param| Some((source_key(param.source?), *params.get(&param.name)?)))
            .collect()
    }

    /// What `entity`'s `action` binds when it moves with `params`, by source.
    pub(super) fn bound_values(
        &self,
        entity: &str,
        action: &str,
        params: &Fields<ParamValue>,
    ) -> Fields<ParamValue> {
        self.models[entity]
            .transitions
            .iter()
            .filter(|t| t.name == action)
            .fold(Fields::new(), |mut bound, t| {
                let by = Self::bound_by(t, params);
                bound.extend(by.iter().map(|(source, value)| (*source, *value)));
                bound
            })
    }

    /// Whether the kernel surely binds what `entity`'s `action` binds when
    /// `by` makes it: it binds nothing, or `by` surely names a caller. The
    /// kernel refuses the move when the chain names none.
    pub(super) fn binds_surely(&self, entity: &str, action: &str, by: u32) -> bool {
        self.senders.names_a_caller(by) == May::Yes || !self.binds(entity, action)
    }

    /// Whether `entity`'s `action` binds a parameter.
    pub(super) fn binds(&self, entity: &str, action: &str) -> bool {
        self.models[entity]
            .transitions
            .iter()
            .any(|t| t.name == action && !t.bound.is_empty())
    }
}

/// `params` with each choice of the kernel's bound values, where `t`'s
/// constraints hold.
pub(super) fn with_callers(
    params: &Fields<ParamValue>,
    callers: Vec<Fields<ParamValue>>,
    t: &ResolvedTransition,
    target: &TemperModelState,
) -> Vec<Fields<ParamValue>> {
    callers
        .into_iter()
        .map(|caller| {
            let mut params = params.clone();
            params.extend(caller.iter().map(|(name, value)| (*name, *value)));
            params
        })
        .filter(|params| constraints_hold(&t.string_params, target, params))
        .collect()
}
