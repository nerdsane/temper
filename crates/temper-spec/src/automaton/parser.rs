//! Parse I/O Automaton TOML specifications.
//!
//! Also provides conversion to the existing TemperModel and TransitionTable
//! formats, so the verification cascade and runtime work unchanged.
//!
//! Reading the TOML document into an [`Automaton`] lives in [`super::toml_parser`] to keep this
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
    super::validate::validate(&automaton)?;
    // ADR-0049: wire each state_timeout's `state` into the target action's
    // `from` list so the action is actually enabled from that state.
    wire_state_timeout_from_states(&mut automaton);
    // ADR-0046/0078: derive the WASM/adapter dispatch records from
    // external `[[action.triggers]]` blocks; translation emits a dispatch per
    // trigger (`translate::dispatch_effects`). Entity-kind triggers are handled
    // separately by the reaction dispatcher.
    expand_external_action_triggers(&mut automaton)?;
    // ADR-0050: enforce (or warn on) liveness coverage.
    check_liveness_coverage(&automaton, mode)?;
    Ok(automaton)
}

/// Name of the dispatch record synthesized for an external trigger. The
/// transition emits it as a custom effect (see
/// [`super::translate::dispatch_effects`]); the runtime's WASM/adapter
/// dispatchers look the record up by it.
pub(crate) fn synthesized_trigger_name(action_name: &str, trigger_name: &str) -> String {
    format!("__trigger__:{action_name}:{trigger_name}")
}

/// ADR-0046/0078: derive the runtime's dispatch records
/// (`automaton.integrations`) from external `[[action.triggers]]` blocks
/// (wasm / adapter), copying module / adapter / config / on_success /
/// on_failure. A record is named after its trigger
/// (logs, governance decisions) and dispatched by its synthesized
/// `trigger` key. Entity-kind triggers dispatch through
/// the reaction system and hook-kind triggers through the host's hook
/// handler, so neither needs a record.
fn expand_external_action_triggers(automaton: &mut Automaton) -> Result<(), AutomatonParseError> {
    use super::types::{Integration, TriggerKind};

    let mut synthesized: Vec<Integration> = Vec::new();
    for action in &automaton.actions {
        for trigger in &action.triggers {
            let synth_name = synthesized_trigger_name(&action.name, &trigger.name);
            match trigger.kind {
                // Entity triggers dispatch through the reaction system, hook
                // triggers through the host; validation refuses webhook
                // triggers, which nothing sends (ADR-0182).
                TriggerKind::Entity | TriggerKind::Hook | TriggerKind::Webhook => continue,
                TriggerKind::Wasm => {
                    let Some(module) = trigger.module.as_ref() else {
                        continue;
                    };
                    synthesized.push(Integration {
                        name: trigger.name.clone(),
                        trigger: synth_name.clone(),
                        integration_type: "wasm".to_string(),
                        module: Some(module.clone()),
                        on_success: trigger.on_success.clone(),
                        on_failure: trigger.on_failure.clone(),
                        llm: trigger.llm,
                        config: trigger.config.clone(),
                    });
                }
                TriggerKind::Adapter => {
                    let mut config = trigger.config.clone();
                    if let Some(adapter) = &trigger.adapter {
                        config.insert("adapter".to_string(), adapter.clone());
                    }
                    synthesized.push(Integration {
                        name: trigger.name.clone(),
                        trigger: synth_name.clone(),
                        integration_type: "adapter".to_string(),
                        module: None,
                        on_success: trigger.on_success.clone(),
                        on_failure: trigger.on_failure.clone(),
                        llm: trigger.llm,
                        config,
                    });
                }
            }
        }
    }
    automaton.integrations.extend(synthesized);
    Ok(())
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
        .filter(|a| a.kind != ActionKind::Output) // Output actions don't transition state
        .map(|a| {
            let from_states = if a.from.is_empty() {
                // Input actions are always enabled (I/O automata property)
                if a.kind.enabled_everywhere() {
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
                guard_expr: a.guard.to_string(),
                has_parameters: !a.params.is_empty(),
                effect_expr: format_effects(&a.effect),
            }
        })
        .collect();

    let invariants = automaton
        .invariants
        .iter()
        .map(|inv| TlaInvariant {
            name: inv.name.clone(),
            expr: inv.assert.to_string(),
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

fn format_effects(effects: &[Effect]) -> String {
    effects
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" /\\ ")
}

#[cfg(test)]
#[path = "parser_test.rs"]
mod tests;
