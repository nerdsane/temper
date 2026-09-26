//! Shared IOA-to-intermediate translation layer.
//!
//! Translates `Automaton` actions into [`ResolvedAction`]s with canonical
//! effect representations; guards are already [`Expr`]s. Both `temper-jit` (runtime) and
//! `temper-verify` (model checking) consume this intermediate form,
//! eliminating duplicated translation logic and preventing semantic drift.

use super::types::{Automaton, Effect};
use crate::predicate::Expr;

// ---------------------------------------------------------------------------
// Intermediate effect representation
// ---------------------------------------------------------------------------

/// Canonical effect produced by shared translation.
///
/// Classified into verifiable (state-modifying) and runtime-only categories.
/// `temper-verify` filters out runtime-only effects; `temper-jit` keeps all.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedEffect {
    // -- Verifiable effects (both runtime and verification) --
    /// Increment a counter variable by 1.
    IncrementCounter(String),
    /// Decrement a counter variable by 1.
    DecrementCounter(String),
    /// Increment a counter variable by a numeric action parameter.
    IncrementCounterByParam { var: String, param: String },
    /// Decrement a counter variable by a numeric action parameter.
    DecrementCounterByParam { var: String, param: String },
    /// Set a counter variable from an action param.
    SetCounterFromParam { var: String, param: String },
    /// Set a boolean variable.
    SetBool { var: String, value: bool },
    /// Append a value to a list variable.
    ListAppend(String),
    /// Remove a value from a list variable by index.
    ListRemoveAt(String),

    // -- Runtime-only effects (filtered out during verification) --
    /// Emit a named event.
    Emit(String),
    /// Trigger a named WASM integration.
    Trigger(String),
    /// Schedule a delayed action.
    Schedule { action: String, delay_seconds: u64 },
    /// Schedule an action at an absolute timestamp from an entity field.
    ScheduleAt { action: String, field: String },
    /// Spawn a child entity.
    Spawn {
        entity_type: String,
        entity_id_source: String,
        initial_action: Option<String>,
        store_id_in: Option<String>,
        copy_fields: Option<Vec<String>>,
    },
}

impl ResolvedEffect {
    /// Returns true if this effect modifies verifiable state (counters, booleans, lists).
    ///
    /// Runtime-only effects (Emit, Trigger, Schedule, ScheduleAt, Spawn) return false.
    pub fn is_verifiable(&self) -> bool {
        matches!(
            self,
            ResolvedEffect::IncrementCounter(_)
                | ResolvedEffect::DecrementCounter(_)
                | ResolvedEffect::SetBool { .. }
                | ResolvedEffect::ListAppend(_)
                | ResolvedEffect::ListRemoveAt(_)
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
    /// Effects (combined from `to` state change + explicit effects + heuristics).
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
    let counter_vars: Vec<String> = automaton
        .state
        .iter()
        .filter(|s| s.var_type == "counter")
        .map(|s| s.name.clone())
        .collect();

    automaton
        .actions
        .iter()
        .filter(|a| a.kind != "output")
        .map(|a| {
            let effects = translate_effects(a.to.as_deref(), &a.effect, &a.name, &counter_vars);

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

/// Translate effects, including state change, explicit effects, and name heuristics.
fn translate_effects(
    _to_state: Option<&str>,
    effects: &[Effect],
    action_name: &str,
    counter_vars: &[String],
) -> Vec<ResolvedEffect> {
    let mut resolved = Vec::new();

    // Explicit effects
    if !effects.is_empty() {
        for e in effects {
            resolved.push(translate_single_effect(e));
        }
    } else {
        // Name-heuristic fallback when no explicit effects are declared.
        apply_name_heuristics(action_name, counter_vars, &mut resolved);
    }

    // Emit event for action (appended by JIT, not by verification).
    // This is left to the consumer since it's a JIT-specific convention.

    resolved
}

/// Translate a single IOA effect to its resolved form.
fn translate_single_effect(effect: &Effect) -> ResolvedEffect {
    match effect {
        Effect::Increment { var, amount } => match amount {
            Some(param) => ResolvedEffect::IncrementCounterByParam {
                var: var.clone(),
                param: param.clone(),
            },
            None => ResolvedEffect::IncrementCounter(var.clone()),
        },
        Effect::Decrement { var, amount } => match amount {
            Some(param) => ResolvedEffect::DecrementCounterByParam {
                var: var.clone(),
                param: param.clone(),
            },
            None => ResolvedEffect::DecrementCounter(var.clone()),
        },
        Effect::SetCounterFromParam { var, param } => ResolvedEffect::SetCounterFromParam {
            var: var.clone(),
            param: param.clone(),
        },
        Effect::SetBool { var, value } => ResolvedEffect::SetBool {
            var: var.clone(),
            value: *value,
        },
        Effect::Emit { event } => ResolvedEffect::Emit(event.clone()),
        Effect::ListAppend { var } => ResolvedEffect::ListAppend(var.clone()),
        Effect::ListRemoveAt { var } => ResolvedEffect::ListRemoveAt(var.clone()),
        Effect::Trigger { name } => ResolvedEffect::Trigger(name.clone()),
        Effect::Schedule {
            action,
            delay_seconds,
        } => ResolvedEffect::Schedule {
            action: action.clone(),
            delay_seconds: *delay_seconds,
        },
        Effect::ScheduleAt { action, field } => ResolvedEffect::ScheduleAt {
            action: action.clone(),
            field: field.clone(),
        },
        Effect::Spawn {
            entity_type,
            entity_id_source,
            initial_action,
            store_id_in,
            copy_fields,
        } => ResolvedEffect::Spawn {
            entity_type: entity_type.clone(),
            entity_id_source: entity_id_source.clone(),
            initial_action: initial_action.clone(),
            store_id_in: store_id_in.clone(),
            copy_fields: copy_fields.clone(),
        },
    }
}

/// Apply name-based heuristics for counter effects.
///
/// When an action has no explicit effects, infers counter increment/decrement
/// from the action name (e.g., "AddItem" → increment all counters).
fn apply_name_heuristics(
    action_name: &str,
    counter_vars: &[String],
    effects: &mut Vec<ResolvedEffect>,
) {
    let name_lower = action_name.to_lowercase();
    if name_lower.contains("additem") || name_lower.contains("add_item") {
        effects.push(ResolvedEffect::IncrementCounter("items".to_string()));
        for var in counter_vars {
            if var != "items" {
                effects.push(ResolvedEffect::IncrementCounter(var.clone()));
            }
        }
    } else if name_lower.contains("removeitem") || name_lower.contains("remove_item") {
        effects.push(ResolvedEffect::DecrementCounter("items".to_string()));
        for var in counter_vars {
            if var != "items" {
                effects.push(ResolvedEffect::DecrementCounter(var.clone()));
            }
        }
    }
}

#[cfg(test)]
#[path = "translate_test.rs"]
mod tests;
