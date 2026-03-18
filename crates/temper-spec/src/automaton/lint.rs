//! Semantic linting for parsed I/O Automata.
//!
//! This pass checks semantic completeness (undefined references, unsupported
//! declarations, and likely-dead transitions) before verification.

use std::collections::{BTreeMap, BTreeSet};

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

/// A semantic lint finding that references a specific entity in a bundle.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BundleLintFinding {
    /// Entity where the issue originates.
    pub entity: String,
    /// Stable lint code for tooling and CI.
    pub code: String,
    /// Error or warning.
    pub severity: LintSeverity,
    /// Human-readable message.
    pub message: String,
}

impl BundleLintFinding {
    fn error(entity: impl Into<String>, code: &str, message: impl Into<String>) -> Self {
        Self {
            entity: entity.into(),
            code: code.to_string(),
            severity: LintSeverity::Error,
            message: message.into(),
        }
    }
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

/// Run semantic lint checks across a bundle of automata.
///
/// Cross-entity checks currently focus on spawn contracts:
/// - spawned target entity must exist in the bundle
/// - target initial action must exist (if configured)
/// - target initial action must be enabled from the target initial state
/// - target initial action params must be available from the spawn action params
///   (plus implicit `parent_type`, `parent_id`, and `<parent_type_snake>_id`)
pub fn lint_automata_bundle(automata: &BTreeMap<String, Automaton>) -> Vec<BundleLintFinding> {
    let mut findings = Vec::new();

    for (entity_name, automaton) in automata {
        let parent_snake = to_snake_case(entity_name);
        for action in &automaton.actions {
            for effect in &action.effect {
                let Effect::Spawn {
                    entity_type,
                    initial_action,
                    ..
                } = effect
                else {
                    continue;
                };

                let Some(target_automaton) = automata.get(entity_type) else {
                    findings.push(BundleLintFinding::error(
                        entity_name.clone(),
                        "spawn_target_missing",
                        format!(
                            "action '{}' spawns unknown entity type '{}'",
                            action.name, entity_type
                        ),
                    ));
                    continue;
                };

                let Some(initial_action_name) = initial_action.as_deref() else {
                    continue;
                };

                let Some(target_action) = target_automaton
                    .actions
                    .iter()
                    .find(|candidate| candidate.name == initial_action_name)
                else {
                    findings.push(BundleLintFinding::error(
                        entity_name.clone(),
                        "spawn_initial_action_missing",
                        format!(
                            "action '{}' spawns '{}' with missing initial_action '{}'",
                            action.name, entity_type, initial_action_name
                        ),
                    ));
                    continue;
                };

                if !target_action.from.is_empty()
                    && !target_action
                        .from
                        .iter()
                        .any(|from| from == &target_automaton.automaton.initial)
                {
                    findings.push(BundleLintFinding::error(
                        entity_name.clone(),
                        "spawn_initial_action_not_from_initial_state",
                        format!(
                            "action '{}' spawns '{}' with initial_action '{}' not enabled from target initial state '{}'",
                            action.name,
                            entity_type,
                            initial_action_name,
                            target_automaton.automaton.initial
                        ),
                    ));
                }

                if target_action.params.is_empty() {
                    continue;
                }

                let mut available_params: BTreeSet<String> =
                    action.params.iter().cloned().collect();
                available_params.insert("parent_id".to_string());
                available_params.insert("parent_type".to_string());
                available_params.insert(format!("{parent_snake}_id"));

                let missing_params: Vec<String> = target_action
                    .params
                    .iter()
                    .filter(|param| !available_params.contains(*param))
                    .cloned()
                    .collect();

                if !missing_params.is_empty() {
                    let available: Vec<String> = available_params.into_iter().collect();
                    findings.push(BundleLintFinding::error(
                        entity_name.clone(),
                        "spawn_initial_action_params_unmapped",
                        format!(
                            "action '{}' spawns '{}' -> '{}'; missing params {:?}, available params {:?}",
                            action.name,
                            entity_type,
                            initial_action_name,
                            missing_params,
                            available
                        ),
                    ));
                }
            }
        }
    }

    findings.sort_by(|a, b| {
        let key_a = (
            &a.entity,
            matches!(a.severity, LintSeverity::Warning),
            &a.code,
            &a.message,
        );
        let key_b = (
            &b.entity,
            matches!(b.severity, LintSeverity::Warning),
            &b.code,
            &b.message,
        );
        key_a.cmp(&key_b)
    });

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
        Guard::IsFalse { var } => Some(var.as_str()),
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
        Guard::IsFalse { var } => format!("is_false {var}"),
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

fn to_snake_case(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    for (index, ch) in value.chars().enumerate() {
        match ch {
            'A'..='Z' => {
                if index > 0 {
                    result.push('_');
                }
                result.push(ch.to_ascii_lowercase());
            }
            '-' | ' ' => result.push('_'),
            _ => result.push(ch.to_ascii_lowercase()),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automaton::parse_automaton;
    use std::collections::BTreeMap;

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

    fn parse(src: &str) -> Automaton {
        parse_automaton(src).expect("parse")
    }

    #[test]
    fn bundle_lint_rejects_missing_spawn_target() {
        let parent = parse(
            r#"
[automaton]
name = "Plan"
states = ["Draft"]
initial = "Draft"

[[action]]
name = "AddTask"
from = ["Draft"]
effect = [{ type = "spawn", entity_type = "Task", entity_id_source = "{uuid}", initial_action = "Create" }]
"#,
        );

        let bundle = BTreeMap::from([("Plan".to_string(), parent)]);
        let findings = lint_automata_bundle(&bundle);
        assert!(findings.iter().any(|f| {
            f.code == "spawn_target_missing"
                && f.entity == "Plan"
                && f.severity == LintSeverity::Error
        }));
    }

    #[test]
    fn bundle_lint_rejects_missing_spawn_initial_action() {
        let parent = parse(
            r#"
[automaton]
name = "Plan"
states = ["Draft"]
initial = "Draft"

[[action]]
name = "AddTask"
from = ["Draft"]
effect = [{ type = "spawn", entity_type = "Task", entity_id_source = "{uuid}", initial_action = "Create" }]
"#,
        );
        let child = parse(
            r#"
[automaton]
name = "Task"
states = ["Open", "Done"]
initial = "Open"

[[action]]
name = "Complete"
from = ["Open"]
to = "Done"
"#,
        );

        let bundle = BTreeMap::from([("Plan".to_string(), parent), ("Task".to_string(), child)]);
        let findings = lint_automata_bundle(&bundle);
        assert!(
            findings
                .iter()
                .any(|f| f.code == "spawn_initial_action_missing" && f.entity == "Plan")
        );
    }

    #[test]
    fn bundle_lint_rejects_spawn_initial_action_not_enabled_from_initial() {
        let parent = parse(
            r#"
[automaton]
name = "Plan"
states = ["Draft"]
initial = "Draft"

[[action]]
name = "AddTask"
from = ["Draft"]
effect = [{ type = "spawn", entity_type = "Task", entity_id_source = "{uuid}", initial_action = "Create" }]
"#,
        );
        let child = parse(
            r#"
[automaton]
name = "Task"
states = ["Open", "InProgress"]
initial = "Open"

[[action]]
name = "Create"
from = ["InProgress"]
"#,
        );

        let bundle = BTreeMap::from([("Plan".to_string(), parent), ("Task".to_string(), child)]);
        let findings = lint_automata_bundle(&bundle);
        assert!(
            findings
                .iter()
                .any(|f| f.code == "spawn_initial_action_not_from_initial_state")
        );
    }

    #[test]
    fn bundle_lint_rejects_unmapped_spawn_params() {
        let parent = parse(
            r#"
[automaton]
name = "Plan"
states = ["Draft"]
initial = "Draft"

[[action]]
name = "AddTask"
from = ["Draft"]
params = ["title"]
effect = [{ type = "spawn", entity_type = "Task", entity_id_source = "{uuid}", initial_action = "Create" }]
"#,
        );
        let child = parse(
            r#"
[automaton]
name = "Task"
states = ["Open"]
initial = "Open"

[[action]]
name = "Create"
from = ["Open"]
params = ["title", "description", "plan_id"]
"#,
        );

        let bundle = BTreeMap::from([("Plan".to_string(), parent), ("Task".to_string(), child)]);
        let findings = lint_automata_bundle(&bundle);
        assert!(findings.iter().any(|f| {
            f.code == "spawn_initial_action_params_unmapped"
                && f.entity == "Plan"
                && f.message.contains("description")
        }));
    }

    #[test]
    fn bundle_lint_accepts_valid_spawn_contract() {
        let parent = parse(
            r#"
[automaton]
name = "Plan"
states = ["Active"]
initial = "Active"

[[action]]
name = "AddTask"
from = ["Active"]
params = ["title", "description"]
effect = [{ type = "spawn", entity_type = "Task", entity_id_source = "{uuid}", initial_action = "Create" }]
"#,
        );
        let child = parse(
            r#"
[automaton]
name = "Task"
states = ["Open"]
initial = "Open"

[[action]]
name = "Create"
from = ["Open"]
params = ["title", "description", "plan_id"]
"#,
        );

        let bundle = BTreeMap::from([("Plan".to_string(), parent), ("Task".to_string(), child)]);
        let findings = lint_automata_bundle(&bundle);
        assert!(
            findings.is_empty(),
            "expected no bundle lint findings, got: {findings:?}"
        );
    }
}
