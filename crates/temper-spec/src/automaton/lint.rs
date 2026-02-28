//! Semantic linting for parsed I/O Automata.
//!
//! This pass checks semantic completeness (undefined references, unsupported
//! declarations, and likely-dead transitions) before verification.

use std::collections::BTreeSet;

use super::{Automaton, Effect, Guard};

/// Severity of a lint finding.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LintSeverity {
    Error,
    Warning,
}

/// A semantic lint finding.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LintFinding {
    /// Stable lint code for tooling and CI.
    pub code: String,
    /// Error or warning.
    pub severity: LintSeverity,
    /// Human-readable message.
    pub message: String,
}

impl LintFinding {
    fn error(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            severity: LintSeverity::Error,
            message: message.into(),
        }
    }

    fn warning(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            severity: LintSeverity::Warning,
            message: message.into(),
        }
    }
}

/// Run semantic lint checks on a parsed automaton.
///
/// This pass is intentionally separate from parsing:
/// - parser: syntax and structural parseability
/// - lint: semantic completeness / consistency
pub fn lint_automaton(automaton: &Automaton) -> Vec<LintFinding> {
    let mut findings = Vec::new();
    let mut vars = BTreeSet::new();

    for state_var in &automaton.state {
        vars.insert(state_var.name.clone());
        if !is_supported_state_var_type(&state_var.var_type) {
            findings.push(LintFinding::error(
                "unknown_state_var_type",
                format!(
                    "state var '{}' has unsupported type '{}'",
                    state_var.name, state_var.var_type
                ),
            ));
        }
    }

    for action in &automaton.actions {
        if action.to.is_none() && action.kind != "output" {
            findings.push(LintFinding::warning(
                "action_missing_to",
                format!(
                    "action '{}' has no `to` target; transition may be dead/no-op",
                    action.name
                ),
            ));
        }

        for guard in &action.guard {
            if let Some(var) = guard_var(guard)
                && !vars.contains(var)
            {
                findings.push(LintFinding::error(
                    "guard_unknown_var",
                    format!(
                        "guard '{}' references unknown variable '{}'",
                        render_guard(guard),
                        var
                    ),
                ));
            }
        }

        for effect in &action.effect {
            if let Some(var) = effect_var(effect)
                && !vars.contains(var)
            {
                findings.push(LintFinding::error(
                    "effect_unknown_var",
                    format!(
                        "effect '{}' references unknown variable '{}'",
                        render_effect(effect),
                        var
                    ),
                ));
            }
        }
    }

    findings
}

fn is_supported_state_var_type(var_type: &str) -> bool {
    matches!(
        var_type,
        "status"
            | "counter"
            | "bool"
            | "set"
            | "list"
            | "string"
            | "int"
            | "integer"
            | "float"
            | "number"
    )
}

fn guard_var(guard: &Guard) -> Option<&str> {
    match guard {
        Guard::StateIn { .. } => None,
        Guard::MinCount { var, .. } => Some(var.as_str()),
        Guard::MaxCount { var, .. } => Some(var.as_str()),
        Guard::IsTrue { var } => Some(var.as_str()),
        Guard::ListContains { var, .. } => Some(var.as_str()),
        Guard::ListLengthMin { var, .. } => Some(var.as_str()),
        Guard::CrossEntityState { .. } => None,
    }
}

fn effect_var(effect: &Effect) -> Option<&str> {
    match effect {
        Effect::Increment { var } => Some(var.as_str()),
        Effect::Decrement { var } => Some(var.as_str()),
        Effect::SetBool { var, .. } => Some(var.as_str()),
        Effect::Emit { .. } => None,
        Effect::ListAppend { var } => Some(var.as_str()),
        Effect::ListRemoveAt { var } => Some(var.as_str()),
        Effect::Trigger { .. } => None,
        Effect::Schedule { .. } => None,
        Effect::Spawn { .. } => None,
    }
}

fn render_guard(guard: &Guard) -> String {
    match guard {
        Guard::StateIn { values } => format!("state_in {:?}", values),
        Guard::MinCount { var, min } => format!("min {var} {min}"),
        Guard::MaxCount { var, max } => format!("max {var} {max}"),
        Guard::IsTrue { var } => format!("is_true {var}"),
        Guard::ListContains { var, value } => format!("list_contains {var} {value}"),
        Guard::ListLengthMin { var, min } => format!("list_length_min {var} {min}"),
        Guard::CrossEntityState {
            entity_type,
            entity_id_source,
            required_status,
        } => {
            format!(
                "cross_entity_state {entity_type}.{entity_id_source} in {:?}",
                required_status
            )
        }
    }
}

fn render_effect(effect: &Effect) -> String {
    match effect {
        Effect::Increment { var } => format!("increment {var}"),
        Effect::Decrement { var } => format!("decrement {var}"),
        Effect::SetBool { var, value } => format!("set {var} {value}"),
        Effect::Emit { event } => format!("emit {event}"),
        Effect::ListAppend { var } => format!("list_append {var}"),
        Effect::ListRemoveAt { var } => format!("list_remove_at {var}"),
        Effect::Trigger { name } => format!("trigger {name}"),
        Effect::Schedule {
            action,
            delay_seconds,
        } => format!("schedule {action} {delay_seconds}s"),
        Effect::Spawn {
            entity_type,
            entity_id_source,
            ..
        } => {
            format!("spawn {entity_type} from {entity_id_source}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automaton::parse_automaton;

    #[test]
    fn lint_rejects_unknown_state_var_type() {
        let src = r#"
[automaton]
name = "Task"
states = ["Draft", "Done"]
initial = "Draft"

[[state]]
name = "mystery"
type = "mystery_type"
initial = "0"

[[action]]
name = "Complete"
from = ["Draft"]
to = "Done"
"#;
        let automaton = parse_automaton(src).expect("parse");
        let findings = lint_automaton(&automaton);
        assert!(
            findings
                .iter()
                .any(|f| f.code == "unknown_state_var_type" && f.severity == LintSeverity::Error)
        );
    }

    #[test]
    fn lint_rejects_unknown_guard_and_effect_variables() {
        let src = r#"
[automaton]
name = "Task"
states = ["Draft", "Done"]
initial = "Draft"

[[state]]
name = "approved"
type = "bool"
initial = "false"

[[action]]
name = "Complete"
from = ["Draft"]
to = "Done"
guard = "is_true phantom"
effect = "set ghost true"
"#;
        let automaton = parse_automaton(src).expect("parse");
        let findings = lint_automaton(&automaton);
        assert!(
            findings
                .iter()
                .any(|f| f.code == "guard_unknown_var" && f.severity == LintSeverity::Error)
        );
        assert!(
            findings
                .iter()
                .any(|f| f.code == "effect_unknown_var" && f.severity == LintSeverity::Error)
        );
    }

    #[test]
    fn lint_warns_for_missing_to_on_internal_action() {
        let src = r#"
[automaton]
name = "Task"
states = ["Draft", "Done"]
initial = "Draft"

[[action]]
name = "Nop"
kind = "internal"
from = ["Draft"]
"#;
        let automaton = parse_automaton(src).expect("parse");
        let findings = lint_automaton(&automaton);
        assert!(
            findings
                .iter()
                .any(|f| f.code == "action_missing_to" && f.severity == LintSeverity::Warning)
        );
    }

    #[test]
    fn lint_allows_missing_to_for_output_action() {
        let src = r#"
[automaton]
name = "Task"
states = ["Draft", "Done"]
initial = "Draft"

[[action]]
name = "EmitAudit"
kind = "output"
from = ["Draft"]
effect = "emit audit"
"#;
        let automaton = parse_automaton(src).expect("parse");
        let findings = lint_automaton(&automaton);
        assert!(!findings.iter().any(|f| f.code == "action_missing_to"));
    }
}
