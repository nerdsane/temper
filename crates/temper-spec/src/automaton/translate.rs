//! Shared IOA-to-intermediate translation layer.
//!
//! Translates `Automaton` actions into [`ResolvedAction`]s with canonical
//! effect representations; guards are already [`Expr`]s. Both `temper-jit` (runtime) and
//! `temper-verify` (model checking) consume this intermediate form,
//! eliminating duplicated translation logic and preventing semantic drift.

use std::collections::BTreeMap;

use super::types::{Action, ActionKind, Automaton, Effect, TriggerKind};
use crate::predicate::{Arg, AssignOp, Expr, VarKind};

// ---------------------------------------------------------------------------
// Intermediate effect representation
// ---------------------------------------------------------------------------

/// Canonical effect produced by shared translation: an effect statement
/// resolved against the declared variable types, or a post-commit dispatch.
///
/// State effects (counters, bools, strings, lists) are applied by the runtime and
/// modeled by the verifier; the rest only the runtime performs.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedEffect {
    // -- State effects (runtime and verification) --
    /// `var = value` on a counter.
    SetCounter { var: String, value: Arg },
    /// `var += value` on a counter.
    AddCounter { var: String, value: Arg },
    /// `var -= value` on a counter; stops at 0.
    SubCounter { var: String, value: Arg },
    /// `var = value` on a bool.
    SetBool { var: String, value: Arg },
    /// `var = 'literal'` on a string, including the empty string.
    SetString { var: String, value: String },
    /// `append(var, value)`.
    ListAppend { var: String, value: Arg },
    /// `remove_at(var, index)`; out of range is a no-op.
    ListRemoveAt { var: String, index: Arg },
    /// `remove(var, value)`: every element equal to `value`; none is a no-op.
    ListRemove { var: String, value: Arg },

    // -- Runtime-only effects --
    /// Post-commit dispatch by name: an external trigger's dispatch record
    /// (`__trigger__:{action}:{trigger}`) or a platform hook.
    Dispatch(String),
    /// Schedule a delayed action.
    Schedule { action: String, delay_seconds: u64 },
    /// Schedule an action at an absolute timestamp from an entity field.
    ScheduleAt { action: String, field: String },
    /// Spawn a child entity; its id is `id` when given and present,
    /// otherwise fresh.
    Spawn {
        entity_type: String,
        initial_action: String,
        store_id_in: Option<String>,
        id: Option<Arg>,
        copy: Vec<String>,
    },
}

impl ResolvedEffect {
    /// Whether this effect writes modeled state (counters, bools, strings, lists).
    pub fn is_state_effect(&self) -> bool {
        matches!(
            self,
            ResolvedEffect::SetCounter { .. }
                | ResolvedEffect::AddCounter { .. }
                | ResolvedEffect::SubCounter { .. }
                | ResolvedEffect::SetBool { .. }
                | ResolvedEffect::SetString { .. }
                | ResolvedEffect::ListAppend { .. }
                | ResolvedEffect::ListRemoveAt { .. }
                | ResolvedEffect::ListRemove { .. }
        )
    }
}

// ---------------------------------------------------------------------------
// Resolved action
// ---------------------------------------------------------------------------

/// A fully resolved action from IOA translation.
///
/// Contains the canonical guard and effects for a single action,
/// ready for consumption by JIT or verification builders.
#[derive(Debug, Clone)]
pub struct ResolvedAction {
    /// Action name (e.g., "SubmitOrder").
    pub name: String,
    /// States from which this action can fire (empty: any state).
    pub from_states: Vec<String>,
    /// Target state after the action fires (if deterministic).
    pub to_state: Option<String>,
    /// Precondition beyond `from_states`.
    pub guard: Expr,
    /// The action's effect statements, then its dispatches.
    pub effects: Vec<ResolvedEffect>,
}

// ---------------------------------------------------------------------------
// Translation functions
// ---------------------------------------------------------------------------

/// Translate all non-output actions from an [`Automaton`] into [`ResolvedAction`]s.
///
/// This is the single source of truth for IOA → intermediate translation.
/// Both JIT and verification builders should call this instead of implementing
/// their own guard/effect matching.
pub fn translate_actions(automaton: &Automaton) -> Vec<ResolvedAction> {
    let kinds: BTreeMap<&str, VarKind> = automaton
        .state
        .iter()
        .map(|s| (s.name.as_str(), s.var_type.kind()))
        .collect();

    automaton
        .actions
        .iter()
        .filter(|a| a.kind != ActionKind::Output)
        .map(|a| {
            let mut effects: Vec<ResolvedEffect> =
                a.effect.iter().map(|e| resolve_effect(e, &kinds)).collect();
            effects.extend(dispatch_effects(a));
            ResolvedAction {
                name: a.name.clone(),
                from_states: a.from.clone(),
                to_state: a.to.clone(),
                guard: a.guard.clone(),
                effects,
            }
        })
        .collect()
}

/// Translate each `[[carry]]` into the move that applies it: from its `from`
/// state to its target, under its guard, with `var = params.var` for each
/// counter and flag it sets. The other values it sets reach the record as
/// the move's params, as any move's params do.
pub fn translate_carries(automaton: &Automaton) -> Vec<ResolvedAction> {
    let kinds: BTreeMap<&str, VarKind> = automaton
        .state
        .iter()
        .map(|s| (s.name.as_str(), s.var_type.kind()))
        .collect();
    automaton
        .carry_moves()
        .into_iter()
        .map(|(name, carry)| ResolvedAction {
            name,
            from_states: vec![carry.from.clone()],
            to_state: Some(carry.target().to_string()),
            guard: carry.guard.clone(),
            effects: carry
                .set
                .keys()
                .filter_map(|var| {
                    let value = Arg::Param(var.clone());
                    match kinds.get(var.as_str()) {
                        Some(VarKind::Counter) => Some(ResolvedEffect::SetCounter {
                            var: var.clone(),
                            value,
                        }),
                        Some(VarKind::Bool) => Some(ResolvedEffect::SetBool {
                            var: var.clone(),
                            value,
                        }),
                        _ => None,
                    }
                })
                .collect(),
        })
        .collect()
}

/// Resolve one (already type-checked) effect statement.
fn resolve_effect(effect: &Effect, kinds: &BTreeMap<&str, VarKind>) -> ResolvedEffect {
    match effect.clone() {
        Effect::Assign { var, op, value } => {
            if kinds.get(var.as_str()) == Some(&VarKind::Bool) {
                debug_assert_eq!(op, AssignOp::Set, "checked at load");
                return ResolvedEffect::SetBool { var, value };
            }
            if kinds.get(var.as_str()) == Some(&VarKind::Str) {
                let Arg::Lit(crate::predicate::Literal::Str(value)) = value else {
                    unreachable!("string assignments are checked at load");
                };
                return ResolvedEffect::SetString { var, value };
            }
            match op {
                AssignOp::Set => ResolvedEffect::SetCounter { var, value },
                AssignOp::Add => ResolvedEffect::AddCounter { var, value },
                AssignOp::Sub => ResolvedEffect::SubCounter { var, value },
            }
        }
        Effect::Append { list, value } => ResolvedEffect::ListAppend { var: list, value },
        Effect::RemoveAt { list, index } => ResolvedEffect::ListRemoveAt { var: list, index },
        Effect::Remove { list, value } => ResolvedEffect::ListRemove { var: list, value },
        Effect::Schedule {
            action,
            delay_seconds,
        } => ResolvedEffect::Schedule {
            action,
            delay_seconds,
        },
        Effect::ScheduleAt { action, field } => ResolvedEffect::ScheduleAt { action, field },
        Effect::Spawn {
            entity_type,
            initial_action,
            store_id_in,
            id,
            copy,
        } => ResolvedEffect::Spawn {
            entity_type,
            initial_action,
            store_id_in,
            id,
            copy,
        },
    }
}

/// The post-commit dispatches an action's `[[action.triggers]]` produce, in
/// declaration order. Entity-kind triggers dispatch through the reaction
/// system instead.
pub fn dispatch_effects(action: &Action) -> Vec<ResolvedEffect> {
    action
        .triggers
        .iter()
        .filter_map(|trigger| match trigger.kind {
            TriggerKind::Entity => None,
            TriggerKind::Hook => trigger.hook.clone().map(ResolvedEffect::Dispatch),
            TriggerKind::Wasm | TriggerKind::Adapter | TriggerKind::Webhook => {
                Some(ResolvedEffect::Dispatch(
                    super::parser::synthesized_trigger_name(&action.name, &trigger.name),
                ))
            }
        })
        .collect()
}

#[cfg(test)]
#[path = "translate_test.rs"]
mod tests;
