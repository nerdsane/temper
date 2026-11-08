//! The `action_param_lost` lint: a param whose value is replaced by the record's
//! own state of the same name.
//!
//! After a move the kernel copies its params into the record's fields, then
//! copies the counters, booleans and lists the record holds over them. So when
//! a record holds `x`, a param `x` that nothing on the action reads through
//! `params.x` is lost: the call answers 200, and fields and modules see the
//! stored value. A string or int state variable is not held that way, so a
//! param of its name lands in fields.

use super::LintFinding;
use crate::automaton::types::Action;
use crate::automaton::{ActionParam, Automaton};
use crate::predicate::{Arg, Effect, VarKind};

/// Find params on `automaton`'s actions whose values are lost.
///
/// Installs and upgrades refuse these. Registry loading only logs them, so boot
/// can restore specs installed before this check existed.
pub fn lint_lost_params(automaton: &Automaton) -> Vec<LintFinding> {
    let mut findings = Vec::new();
    for action in &automaton.actions {
        for param in &action.params {
            if let Some(kind) = held_kind(automaton, param)
                && !reads_param(action, param.name())
            {
                findings.push(lost(action, param.name(), kind));
            }
        }
    }
    findings
}

/// The kind of the state variable this param's value would lose to, if the
/// record holds one by that name: a strict or constrained type holds its
/// declared counters and booleans from creation, and any type holds a variable
/// once something writes it.
fn held_kind(automaton: &Automaton, param: &ActionParam) -> Option<VarKind> {
    let var = automaton.state.iter().find(|v| v.name == param.name())?;
    let kind = var.var_type.kind();
    if !matches!(kind, VarKind::Counter | VarKind::Bool | VarKind::List) {
        return None;
    }
    let seeded = (automaton.automaton.strict_action_params
        || automaton.actions.iter().any(|a| !a.constraints.is_empty()))
        && kind != VarKind::List;
    let written = automaton
        .actions
        .iter()
        .flat_map(|a| &a.effect)
        .any(|effect| writes(effect, &var.name))
        || automaton
            .carries
            .iter()
            .any(|carry| carry.set.contains_key(&var.name));
    (seeded || written).then_some(kind)
}

fn writes(effect: &Effect, name: &str) -> bool {
    match effect {
        Effect::Assign { var, .. } => var == name,
        Effect::Append { list, .. }
        | Effect::RemoveAt { list, .. }
        | Effect::Remove { list, .. } => list == name,
        Effect::Schedule { .. } | Effect::ScheduleAt { .. } | Effect::Spawn { .. } => false,
    }
}

/// Whether the action uses the value it was sent: through `params.name` in its
/// guard, an effect or a trigger, in a constraint, or by a spawn, which hands
/// the child every param.
fn reads_param(action: &Action, name: &str) -> bool {
    let is_param = |arg: &Arg| matches!(arg, Arg::Param(param) if param == name);
    let in_effect = |effect: &Effect| match effect {
        Effect::Assign { value, .. }
        | Effect::Append { value, .. }
        | Effect::Remove { value, .. } => is_param(value),
        Effect::RemoveAt { index, .. } => is_param(index),
        Effect::Spawn { .. } => true,
        Effect::Schedule { .. } | Effect::ScheduleAt { .. } => false,
    };
    action.guard.params_read().iter().any(|p| p == name)
        || action.constraints.iter().any(|c| c.param() == name)
        || action.effect.iter().any(in_effect)
        || action.triggers.iter().any(|trigger| {
            trigger.args.values().any(is_param)
                || trigger
                    .guard
                    .as_ref()
                    .is_some_and(|guard| guard.params_read().iter().any(|p| p == name))
        })
}

fn lost(action: &Action, name: &str, kind: VarKind) -> LintFinding {
    LintFinding::error(
        "action_param_lost",
        format!(
            "action '{action}' takes param '{name}', but the record's own '{name}' ({kind:?}) \
             replaces it after the move, so the value sent is read by nothing. To store it, add \
             the effect `{name} = params.{name}`. Otherwise rename the param, or drop it or the \
             [[state]]",
            action = action.name,
        ),
    )
}

#[cfg(test)]
#[path = "lost_params_test.rs"]
mod tests;
