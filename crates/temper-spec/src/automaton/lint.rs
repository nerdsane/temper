//! Semantic linting for parsed I/O Automata.
//!
//! This pass checks semantic completeness (undefined references, unsupported
//! declarations, and likely-dead transitions) before verification.

use std::collections::{BTreeMap, BTreeSet};

use super::{Automaton, Effect, FieldInvariant};

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

    for state_var in &automaton.state {
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
    }

    lint_field_invariants(automaton, &mut findings);

    findings
}

/// Validate parsed `[[field_invariant]]` entries: names must be present and
/// unique, and an `a == x => a == y` rule with `x != y` can never pass.
fn lint_field_invariants(automaton: &Automaton, findings: &mut Vec<LintFinding>) {
    let mut seen_names: BTreeSet<&str> = BTreeSet::new();
    for inv in &automaton.field_invariants {
        if inv.name.trim().is_empty() {
            findings.push(LintFinding::error(
                "field_invariant_missing_name",
                "field_invariant has empty `name` — error responses would have no identifier",
            ));
        } else if !seen_names.insert(inv.name.as_str()) {
            findings.push(LintFinding::error(
                "field_invariant_duplicate_name",
                format!("field_invariant '{}' is declared more than once", inv.name),
            ));
        }
        check_unsatisfiable_same_field_equals(inv, findings);
    }
}

/// Detect the simplest class of trivially-unsatisfiable invariants:
/// `x == a => x == b` with `a != b`. Every write matching the left side
/// would be rejected.
fn check_unsatisfiable_same_field_equals(inv: &FieldInvariant, findings: &mut Vec<LintFinding>) {
    use crate::predicate::{CmpOp, Expr, Operand};
    let equality = |expr: &Expr| match expr {
        Expr::Compare {
            lhs: Operand::Var(field),
            op: CmpOp::Eq,
            rhs: Operand::Lit(lit),
        } => Some((field.clone(), lit.clone())),
        _ => None,
    };
    if let Expr::Implies(when, require) = &inv.assert
        && let (Some((lf, lv)), Some((rf, rv))) = (equality(when), equality(require))
        && lf == rf
        && lv != rv
    {
        findings.push(LintFinding::warning(
            "field_invariant_trivially_unsatisfiable",
            format!(
                "field_invariant '{}' requires field '{}' to equal both {} and {}",
                inv.name, lf, lv, rv
            ),
        ));
    }
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
                lint_spawn_effect(
                    automata,
                    entity_name,
                    &parent_snake,
                    action,
                    effect,
                    &mut findings,
                );
            }
        }
    }

    sort_bundle_findings(&mut findings);
    findings
}

fn lint_spawn_effect(
    automata: &BTreeMap<String, Automaton>,
    entity_name: &str,
    parent_snake: &str,
    action: &super::Action,
    effect: &Effect,
    findings: &mut Vec<BundleLintFinding>,
) {
    let Effect::Spawn {
        entity_type,
        initial_action,
        ..
    } = effect
    else {
        return;
    };

    let Some(target_automaton) = automata.get(entity_type) else {
        findings.push(BundleLintFinding::error(
            entity_name.to_string(),
            "spawn_target_missing",
            format!(
                "action '{}' spawns unknown entity type '{}'",
                action.name, entity_type
            ),
        ));
        return;
    };

    let initial_action_name = initial_action.as_str();

    let Some(target_action) = target_action(target_automaton, initial_action_name) else {
        findings.push(BundleLintFinding::error(
            entity_name.to_string(),
            "spawn_initial_action_missing",
            format!(
                "action '{}' spawns '{}' with missing initial_action '{}'",
                action.name, entity_type, initial_action_name
            ),
        ));
        return;
    };

    lint_spawn_initial_state(
        entity_name,
        action,
        entity_type,
        initial_action_name,
        target_automaton,
        target_action,
        findings,
    );
    let available_params = available_spawn_params(action, parent_snake);
    lint_spawn_param_mapping(
        entity_name,
        &action.name,
        &available_params,
        entity_type,
        initial_action_name,
        target_action,
        findings,
    );
}

fn target_action<'a>(automaton: &'a Automaton, action_name: &str) -> Option<&'a super::Action> {
    automaton
        .actions
        .iter()
        .find(|candidate| candidate.name == action_name)
}

fn lint_spawn_initial_state(
    entity_name: &str,
    action: &super::Action,
    entity_type: &str,
    initial_action_name: &str,
    target_automaton: &Automaton,
    target_action: &super::Action,
    findings: &mut Vec<BundleLintFinding>,
) {
    if target_action.from.is_empty()
        || target_action
            .from
            .iter()
            .any(|from| from == &target_automaton.automaton.initial)
    {
        return;
    }

    findings.push(BundleLintFinding::error(
        entity_name.to_string(),
        "spawn_initial_action_not_from_initial_state",
        format!(
            "action '{}' spawns '{}' with initial_action '{}' not enabled from target initial state '{}'",
            action.name, entity_type, initial_action_name, target_automaton.automaton.initial
        ),
    ));
}

fn lint_spawn_param_mapping(
    entity_name: &str,
    action_name: &str,
    available_params: &BTreeSet<String>,
    entity_type: &str,
    initial_action_name: &str,
    target_action: &super::Action,
    findings: &mut Vec<BundleLintFinding>,
) {
    if target_action.params.is_empty() {
        return;
    }

    let missing_params: Vec<String> = target_action
        .params
        .iter()
        .map(|p| p.name().to_string())
        .filter(|param| !available_params.contains(param))
        .collect();

    if missing_params.is_empty() {
        return;
    }

    let available: Vec<String> = available_params.iter().cloned().collect();
    findings.push(BundleLintFinding::error(
        entity_name.to_string(),
        "spawn_initial_action_params_unmapped",
        format!(
            "action '{}' spawns '{}' -> '{}'; missing params {:?}, available params {:?}",
            action_name, entity_type, initial_action_name, missing_params, available
        ),
    ));
}

fn available_spawn_params(action: &super::Action, parent_snake: &str) -> BTreeSet<String> {
    let mut available_params: BTreeSet<String> =
        action.params.iter().map(|p| p.name().to_string()).collect();
    available_params.insert("parent_id".to_string());
    available_params.insert("parent_type".to_string());
    available_params.insert(format!("{parent_snake}_id"));
    available_params
}

fn sort_bundle_findings(findings: &mut [BundleLintFinding]) {
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
#[path = "lint_test.rs"]
mod tests;
