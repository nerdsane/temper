//! Parse I/O Automaton TOML specifications.
//!
//! Also provides conversion to the existing TemperModel and TransitionTable
//! formats, so the verification cascade and runtime work unchanged.
//!
//! The hand-rolled TOML parser lives in [`super::toml_parser`] to keep this
//! module focused on the public API and validation logic.

use super::toml_parser;
use super::types::*;
use crate::tlaplus::{Invariant as TlaInvariant, StateMachine, Transition};

/// Errors from parsing an automaton specification.
#[derive(Debug, thiserror::Error)]
pub enum AutomatonParseError {
    #[error("TOML parse error: {0}")]
    Toml(String),
    #[error("validation error: {0}")]
    Validation(String),
}

/// Liveness enforcement mode (ADR-0050).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LivenessEnforcement {
    /// Missing coverage logs `tracing::warn!` and the spec loads. Default
    /// during rollout.
    WarnOnly,
    /// Missing coverage produces `AutomatonParseError::Validation`.
    Enforce,
}

impl LivenessEnforcement {
    /// Read the mode from `TEMPER_LIVENESS_ENFORCE`. Recognized truthy
    /// values: `"1"`, `"true"`, `"on"`, `"yes"` (case-insensitive).
    pub fn from_env() -> Self {
        // determinism-ok: env read once at parse time; deterministic under DST
        // because the env var is set before the simulation begins.
        let enforce = std::env::var("TEMPER_LIVENESS_ENFORCE")
            .ok()
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "on" | "yes"
                )
            })
            .unwrap_or(false);
        if enforce {
            Self::Enforce
        } else {
            Self::WarnOnly
        }
    }
}

/// Parse an I/O Automaton specification from TOML.
///
/// Liveness coverage (ADR-0050) is checked in the mode specified by
/// `TEMPER_LIVENESS_ENFORCE` (default: warn-only). For deterministic tests,
/// use [`parse_automaton_with_liveness`].
pub fn parse_automaton(toml_str: &str) -> Result<Automaton, AutomatonParseError> {
    parse_automaton_with_liveness(toml_str, LivenessEnforcement::from_env())
}

/// Parse with an explicit liveness enforcement mode. Tests should call this
/// directly so they do not rely on process-global env state.
pub fn parse_automaton_with_liveness(
    toml_str: &str,
    mode: LivenessEnforcement,
) -> Result<Automaton, AutomatonParseError> {
    let mut automaton: Automaton = toml_parser::parse_toml_to_automaton(toml_str)?;
    validate(&automaton)?;
    // ADR-0049: wire each state_timeout's `state` into the target action's
    // `from` list so the action is actually enabled from that state.
    wire_state_timeout_from_states(&mut automaton);
    // ADR-0050: enforce (or warn on) liveness coverage.
    check_liveness_coverage(&automaton, mode)?;
    Ok(automaton)
}

/// Callback invoked for each liveness violation encountered at spec parse
/// time. Allows downstream crates to emit metrics (ADR-0050) without
/// temper-spec taking a dependency on an observability stack.
pub type LivenessViolationReporter =
    dyn Fn(&super::metadata::LivenessViolation) + Send + Sync + 'static;

use std::sync::OnceLock;
static VIOLATION_REPORTER: OnceLock<Box<LivenessViolationReporter>> = OnceLock::new();

/// Install a global reporter used whenever a liveness violation is observed
/// during `parse_automaton*`. Callable at most once per process.
pub fn set_liveness_violation_reporter<F>(reporter: F)
where
    F: Fn(&super::metadata::LivenessViolation) + Send + Sync + 'static,
{
    let _ = VIOLATION_REPORTER.set(Box::new(reporter));
}

fn check_liveness_coverage(
    automaton: &Automaton,
    mode: LivenessEnforcement,
) -> Result<(), AutomatonParseError> {
    let Err(violations) = automaton.validate_liveness_coverage() else {
        return Ok(());
    };

    if let Some(reporter) = VIOLATION_REPORTER.get() {
        for v in &violations {
            reporter(v);
        }
    }

    match mode {
        LivenessEnforcement::Enforce => {
            let summary = violations
                .iter()
                .map(|v| format!("  - {v}"))
                .collect::<Vec<_>>()
                .join("\n");
            Err(AutomatonParseError::Validation(format!(
                "{} non-terminal state(s) missing liveness coverage (ADR-0050):\n{summary}",
                violations.len()
            )))
        }
        LivenessEnforcement::WarnOnly => {
            for v in &violations {
                tracing::warn!(
                    entity = %v.entity,
                    state = %v.state,
                    "liveness coverage missing (ADR-0050): non-terminal state has no state_timeout and is not in allow_indefinite_states"
                );
            }
            Ok(())
        }
    }
}

/// Ensure each `[[state_timeout]]` target action has the timer's `state` in
/// its `from` list. Safe to call after `validate` has confirmed every
/// `on_timeout` references an existing action.
fn wire_state_timeout_from_states(automaton: &mut Automaton) {
    // Build a (action_name -> target states) map so we touch each action once.
    let mut to_add: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for st in &automaton.state_timeouts {
        to_add
            .entry(st.on_timeout.clone())
            .or_default()
            .push(st.state.clone());
    }

    for action in automaton.actions.iter_mut() {
        if let Some(states) = to_add.get(&action.name) {
            for s in states {
                if !action.from.contains(s) {
                    action.from.push(s.clone());
                }
            }
        }
    }
}

/// Convert an I/O Automaton to the legacy StateMachine format.
///
/// This allows the existing verification cascade (Stateright, DST, proptest)
/// and the TransitionTable builder to work unchanged.
pub fn to_state_machine(automaton: &Automaton) -> StateMachine {
    let transitions = automaton
        .actions
        .iter()
        .filter(|a| a.kind != "output") // Output actions don't transition state
        .map(|a| {
            let from_states = if a.from.is_empty() {
                // Input actions are always enabled (I/O automata property)
                if a.kind == "input" {
                    automaton.automaton.states.clone()
                } else {
                    vec![]
                }
            } else {
                a.from.clone()
            };

            Transition {
                name: a.name.clone(),
                from_states,
                to_state: a.to.clone(),
                guard_expr: format_guards(&a.guard),
                has_parameters: !a.params.is_empty(),
                effect_expr: format_effects(&a.effect),
            }
        })
        .collect();

    let invariants = automaton
        .invariants
        .iter()
        .map(|inv| {
            // Encode `when` states as a trigger prefix so the model builder
            // can extract trigger_states via extract_trigger_states().
            let trigger = if inv.when.is_empty() {
                String::new()
            } else {
                let states = inv
                    .when
                    .iter()
                    .map(|s| format!("\"{s}\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("status \\in {{{states}}} => ")
            };
            TlaInvariant {
                name: inv.name.clone(),
                expr: format!("{trigger}{}", inv.assert),
            }
        })
        .collect();

    StateMachine {
        module_name: automaton.automaton.name.clone(),
        states: automaton.automaton.states.clone(),
        transitions,
        invariants,
        liveness_properties: vec![],
        constants: vec![],
        variables: automaton.state.iter().map(|s| s.name.clone()).collect(),
    }
}

fn format_guards(guards: &[Guard]) -> String {
    guards
        .iter()
        .map(|g| match g {
            Guard::StateIn { values } => {
                format!(
                    "status \\in {{{}}}",
                    values
                        .iter()
                        .map(|s| format!("\"{s}\""))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            Guard::MinCount { var, min } => format!("{var} >= {min}"),
            Guard::MaxCount { var, max } => format!("{var} < {max}"),
            Guard::IsTrue { var } => format!("{var} = TRUE"),
            Guard::IsFalse { var } => format!("{var} = FALSE"),
            Guard::ListContains { var, value } => format!("{value} \\in {var}"),
            Guard::ListLengthMin { var, min } => format!("Len({var}) >= {min}"),
            Guard::CrossEntityState {
                entity_type,
                entity_id_source,
                required_status,
            } => {
                format!(
                    "{entity_type}[{entity_id_source}].status \\in {{{}}}",
                    required_status
                        .iter()
                        .map(|s| format!("\"{s}\""))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        })
        .collect::<Vec<_>>()
        .join(" /\\ ")
}

fn format_effects(effects: &[Effect]) -> String {
    effects
        .iter()
        .map(|e| match e {
            Effect::Increment { var } => format!("{var}' = {var} + 1"),
            Effect::Decrement { var } => format!("{var}' = {var} - 1"),
            Effect::SetBool { var, value } => {
                format!("{var}' = {}", if *value { "TRUE" } else { "FALSE" })
            }
            Effect::Emit { event } => format!("Emit(\"{event}\")"),
            Effect::Trigger { name } => format!("Trigger(\"{name}\")"),
            Effect::Schedule {
                action,
                delay_seconds,
            } => format!("Schedule(\"{action}\", {delay_seconds})"),
            Effect::ListAppend { var } => format!("ListAppend({var})"),
            Effect::ListRemoveAt { var } => format!("ListRemoveAt({var})"),
            Effect::ScheduleAt { action, field } => {
                format!("ScheduleAt(\"{action}\", {field})")
            }
            Effect::Spawn {
                entity_type,
                entity_id_source,
                ..
            } => {
                format!("Spawn({entity_type}, {entity_id_source})")
            }
        })
        .collect::<Vec<_>>()
        .join(" /\\ ")
}

fn validate(automaton: &Automaton) -> Result<(), AutomatonParseError> {
    // 1. Initial state must be in the states list.
    if !automaton
        .automaton
        .states
        .contains(&automaton.automaton.initial)
    {
        return Err(AutomatonParseError::Validation(format!(
            "initial state '{}' is not in states list",
            automaton.automaton.initial
        )));
    }

    // 2. All `from` and `to` states in actions must be declared states.
    for action in &automaton.actions {
        for from in &action.from {
            if !automaton.automaton.states.contains(from) {
                return Err(AutomatonParseError::Validation(format!(
                    "action '{}' references undeclared from-state '{from}'",
                    action.name
                )));
            }
        }
        if let Some(to) = &action.to
            && !automaton.automaton.states.contains(to)
        {
            return Err(AutomatonParseError::Validation(format!(
                "action '{}' references undeclared to-state '{to}'",
                action.name
            )));
        }
    }

    // 3. Validate WASM integrations.
    let action_names: Vec<&str> = automaton.actions.iter().map(|a| a.name.as_str()).collect();
    for ig in &automaton.integrations {
        if ig.integration_type == "wasm" {
            if ig.module.is_none() {
                return Err(AutomatonParseError::Validation(format!(
                    "integration '{}' is type 'wasm' but missing 'module' field",
                    ig.name
                )));
            }
            if let Some(ref cb) = ig.on_success
                && !action_names.contains(&cb.as_str())
            {
                return Err(AutomatonParseError::Validation(format!(
                    "integration '{}' on_success references unknown action '{cb}'",
                    ig.name
                )));
            }
            if let Some(ref cb) = ig.on_failure
                && !action_names.contains(&cb.as_str())
            {
                return Err(AutomatonParseError::Validation(format!(
                    "integration '{}' on_failure references unknown action '{cb}'",
                    ig.name
                )));
            }
        }
    }

    // 4. Validate [[state_timeout]] declarations (ADR-0049).
    //    - `state` must be a declared state.
    //    - `on_timeout` must be a declared action.
    //    - each `reset_on` entry must be a declared action.
    //    - `after_seconds` must be > 0 (a zero delay is almost certainly a typo).
    //    - `max_occurrences` must be >= 1.
    //    - the same state must not be declared twice (ambiguous timer contract).
    let mut seen_states: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for st in &automaton.state_timeouts {
        if !automaton.automaton.states.contains(&st.state) {
            return Err(AutomatonParseError::Validation(format!(
                "state_timeout references undeclared state '{}'",
                st.state
            )));
        }
        if !seen_states.insert(st.state.as_str()) {
            return Err(AutomatonParseError::Validation(format!(
                "state_timeout declared twice for state '{}'",
                st.state
            )));
        }
        if st.after_seconds == 0 {
            return Err(AutomatonParseError::Validation(format!(
                "state_timeout for '{}' must have after_seconds > 0",
                st.state
            )));
        }
        if st.max_occurrences == 0 {
            return Err(AutomatonParseError::Validation(format!(
                "state_timeout for '{}' must have max_occurrences >= 1",
                st.state
            )));
        }
        if !action_names.contains(&st.on_timeout.as_str()) {
            return Err(AutomatonParseError::Validation(format!(
                "state_timeout for '{}' references unknown on_timeout action '{}'",
                st.state, st.on_timeout
            )));
        }
        for reset in &st.reset_on {
            if !action_names.contains(&reset.as_str()) {
                return Err(AutomatonParseError::Validation(format!(
                    "state_timeout for '{}' references unknown reset_on action '{reset}'",
                    st.state
                )));
            }
        }
    }

    // 5. Validate allow_indefinite_states entries are declared states
    //    (ADR-0050 support).
    for state in &automaton.automaton.allow_indefinite_states {
        if !automaton.automaton.states.contains(state) {
            return Err(AutomatonParseError::Validation(format!(
                "allow_indefinite_states references undeclared state '{state}'"
            )));
        }
    }

    // 6. Validate [[action.triggers]] declarations (ADR-0046).
    validate_action_triggers(automaton, &action_names)?;

    Ok(())
}

/// Validate all `[[action.triggers]]` blocks per ADR-0046 rules.
///
/// Checks performed (parse-time, per-entity — cross-entity checks like
/// target-action existence happen at registry load time):
/// - Kind-specific required fields present.
/// - `to_state` (if set) is a declared state.
/// - Trigger guard nesting depth ≤ `MAX_TRIGGER_GUARD_DEPTH`.
/// - `params` and `params_from` keys must not collide.
/// - For `Wasm`/`Webhook` kinds: `on_success`/`on_failure` reference
///   actions declared on the same source entity.
/// - Trigger names within a single action must be unique.
fn validate_action_triggers(
    automaton: &Automaton,
    action_names: &[&str],
) -> Result<(), AutomatonParseError> {
    for action in &automaton.actions {
        let mut seen_names: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for trigger in &action.triggers {
            if trigger.name.is_empty() {
                return Err(AutomatonParseError::Validation(format!(
                    "action '{}' has an [[action.triggers]] block with empty name",
                    action.name
                )));
            }
            if !seen_names.insert(trigger.name.as_str()) {
                return Err(AutomatonParseError::Validation(format!(
                    "action '{}' declares trigger '{}' more than once",
                    action.name, trigger.name
                )));
            }

            // Kind-specific field presence.
            match trigger.kind {
                TriggerKind::Entity => {
                    if trigger.target_entity.as_deref().is_none_or(str::is_empty) {
                        return Err(AutomatonParseError::Validation(format!(
                            "trigger '{}' on action '{}' is kind=\"entity\" but missing 'target_entity'",
                            trigger.name, action.name
                        )));
                    }
                    if trigger.target_action.as_deref().is_none_or(str::is_empty) {
                        return Err(AutomatonParseError::Validation(format!(
                            "trigger '{}' on action '{}' is kind=\"entity\" but missing 'target_action'",
                            trigger.name, action.name
                        )));
                    }
                    if trigger.resolve_target.is_none() {
                        return Err(AutomatonParseError::Validation(format!(
                            "trigger '{}' on action '{}' is kind=\"entity\" but missing 'resolve_target'",
                            trigger.name, action.name
                        )));
                    }
                }
                TriggerKind::Wasm => {
                    if trigger.module.as_deref().is_none_or(str::is_empty) {
                        return Err(AutomatonParseError::Validation(format!(
                            "trigger '{}' on action '{}' is kind=\"wasm\" but missing 'module'",
                            trigger.name, action.name
                        )));
                    }
                }
                TriggerKind::Webhook => {
                    if trigger.url.as_deref().is_none_or(str::is_empty) {
                        return Err(AutomatonParseError::Validation(format!(
                            "trigger '{}' on action '{}' is kind=\"webhook\" but missing 'url'",
                            trigger.name, action.name
                        )));
                    }
                    if trigger.method.as_deref().is_none_or(str::is_empty) {
                        return Err(AutomatonParseError::Validation(format!(
                            "trigger '{}' on action '{}' is kind=\"webhook\" but missing 'method'",
                            trigger.name, action.name
                        )));
                    }
                }
            }

            // to_state filter must be a declared state if set.
            if let Some(ref to_state) = trigger.to_state
                && !automaton.automaton.states.contains(to_state)
            {
                return Err(AutomatonParseError::Validation(format!(
                    "trigger '{}' on action '{}' references undeclared to_state '{to_state}'",
                    trigger.name, action.name
                )));
            }

            // on_success / on_failure must reference actions declared on this
            // source entity (they dispatch on the source after module/HTTP).
            if let Some(ref cb) = trigger.on_success
                && !action_names.contains(&cb.as_str())
            {
                return Err(AutomatonParseError::Validation(format!(
                    "trigger '{}' on action '{}' on_success references unknown action '{cb}'",
                    trigger.name, action.name
                )));
            }
            if let Some(ref cb) = trigger.on_failure
                && !action_names.contains(&cb.as_str())
            {
                return Err(AutomatonParseError::Validation(format!(
                    "trigger '{}' on action '{}' on_failure references unknown action '{cb}'",
                    trigger.name, action.name
                )));
            }

            // Guard depth bound.
            if let Some(ref guard) = trigger.guard {
                let depth = guard.depth();
                if depth > MAX_TRIGGER_GUARD_DEPTH {
                    return Err(AutomatonParseError::Validation(format!(
                        "trigger '{}' on action '{}' has guard nesting depth {depth} exceeding \
                         MAX_TRIGGER_GUARD_DEPTH ({MAX_TRIGGER_GUARD_DEPTH})",
                        trigger.name, action.name
                    )));
                }
            }

            // params / params_from key collision.
            if let Some(params_obj) = trigger.params.as_object() {
                for key in trigger.params_from.keys() {
                    if params_obj.contains_key(key) {
                        return Err(AutomatonParseError::Validation(format!(
                            "trigger '{}' on action '{}' has key '{key}' in both 'params' and 'params_from'",
                            trigger.name, action.name
                        )));
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
#[path = "parser_test.rs"]
mod tests;
