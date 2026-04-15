//! Shared IOA-to-intermediate translation layer.
//!
//! Translates `Automaton` actions into [`ResolvedAction`]s with canonical
//! guard and effect representations. Both `temper-jit` (runtime) and
//! `temper-verify` (model checking) consume this intermediate form,
//! eliminating duplicated translation logic and preventing semantic drift.

use super::types::{Automaton, Effect, Guard};

// ---------------------------------------------------------------------------
// Intermediate guard representation
// ---------------------------------------------------------------------------

/// Canonical guard produced by shared translation.
///
/// Consumers map this to their domain-specific guard type. For example,
/// `temper-verify` maps `CrossEntityState` → `Always` (permissive),
/// while `temper-jit` maps it to a runtime cross-entity check.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedGuard {
    /// No guard — always passes.
    Always,
    /// Current status must be in the given set.
    StateIn(Vec<String>),
    /// A counter variable must be >= min.
    CounterMin { var: String, min: usize },
    /// A counter variable must be < max.
    CounterMax { var: String, max: usize },
    /// A boolean variable must be true.
    BoolTrue(String),
    /// A boolean variable must be false.
    BoolFalse(String),
    /// A list variable must contain a specific value.
    ListContains { var: String, value: String },
    /// A list variable must have at least N elements.
    ListLengthMin { var: String, min: usize },
    /// Another entity must be in one of the required statuses.
    CrossEntityState {
        entity_type: String,
        entity_id_source: String,
        required_status: Vec<String>,
    },
    /// All inner guards must pass.
    And(Vec<ResolvedGuard>),
}

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
    /// States from which this action can fire.
    pub from_states: Vec<String>,
    /// Target state after the action fires (if deterministic).
    pub to_state: Option<String>,
    /// Guard condition (combined from `from` + explicit guards).
    pub guard: ResolvedGuard,
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
            let guard = translate_guards(&a.from, &a.guard);
            let effects = translate_effects(a.to.as_deref(), &a.effect, &a.name, &counter_vars);

            ResolvedAction {
                name: a.name.clone(),
                from_states: a.from.clone(),
                to_state: a.to.clone(),
                guard,
                effects,
            }
        })
        .collect()
}

/// Translate guard clauses into a single [`ResolvedGuard`].
///
/// Combines `from` states with explicit guard conditions using `And`.
fn translate_guards(from_states: &[String], guards: &[Guard]) -> ResolvedGuard {
    let mut resolved = Vec::new();

    if !from_states.is_empty() {
        resolved.push(ResolvedGuard::StateIn(from_states.to_vec()));
    }

    for g in guards {
        resolved.push(translate_single_guard(g));
    }

    match resolved.len() {
        0 => ResolvedGuard::Always,
        1 => resolved.remove(0),
        _ => ResolvedGuard::And(resolved),
    }
}

/// Translate a single IOA guard to its resolved form.
fn translate_single_guard(guard: &Guard) -> ResolvedGuard {
    match guard {
        Guard::StateIn { values } => ResolvedGuard::StateIn(values.clone()),
        Guard::MinCount { var, min } => ResolvedGuard::CounterMin {
            var: var.clone(),
            min: *min,
        },
        Guard::MaxCount { var, max } => ResolvedGuard::CounterMax {
            var: var.clone(),
            max: *max,
        },
        Guard::IsTrue { var } => ResolvedGuard::BoolTrue(var.clone()),
        Guard::IsFalse { var } => ResolvedGuard::BoolFalse(var.clone()),
        Guard::ListContains { var, value } => ResolvedGuard::ListContains {
            var: var.clone(),
            value: value.clone(),
        },
        Guard::ListLengthMin { var, min } => ResolvedGuard::ListLengthMin {
            var: var.clone(),
            min: *min,
        },
        Guard::CrossEntityState {
            entity_type,
            entity_id_source,
            required_status,
        } => ResolvedGuard::CrossEntityState {
            entity_type: entity_type.clone(),
            entity_id_source: entity_id_source.clone(),
            required_status: required_status.clone(),
        },
    }
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
        Effect::Increment { var } => ResolvedEffect::IncrementCounter(var.clone()),
        Effect::Decrement { var } => ResolvedEffect::DecrementCounter(var.clone()),
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
