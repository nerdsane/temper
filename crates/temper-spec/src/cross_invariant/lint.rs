use std::collections::BTreeSet;

use super::parser::{parse_related_status_in_assert, split_trigger};
use super::types::{CrossInvariantSpec, InvariantKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CrossInvariantLintSeverity {
    Error,
    Warning,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CrossInvariantLintFinding {
    pub code: String,
    pub severity: CrossInvariantLintSeverity,
    pub invariant: Option<String>,
    pub message: String,
}

impl CrossInvariantLintFinding {
    fn error(code: &str, invariant: Option<&str>, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            severity: CrossInvariantLintSeverity::Error,
            invariant: invariant.map(|s| s.to_string()),
            message: message.into(),
        }
    }
}

/// Lint parsed cross-invariant specs for semantic consistency.
pub fn lint_cross_invariants(spec: &CrossInvariantSpec) -> Vec<CrossInvariantLintFinding> {
    let mut findings = Vec::new();
    let mut names = BTreeSet::new();

    for inv in &spec.invariants {
        if !names.insert(inv.name.clone()) {
            findings.push(CrossInvariantLintFinding::error(
                "duplicate_invariant_name",
                Some(&inv.name),
                format!("invariant '{}' is declared more than once", inv.name),
            ));
        }
        if split_trigger(&inv.on).is_none() {
            findings.push(CrossInvariantLintFinding::error(
                "invalid_trigger",
                Some(&inv.name),
                format!("trigger '{}' must be Entity.* or Entity.Action", inv.on),
            ));
        }
        if parse_related_status_in_assert(&inv.assertion).is_none() {
            findings.push(CrossInvariantLintFinding::error(
                "invalid_assertion",
                Some(&inv.name),
                "assertion must be: related(TargetEntity, source_field).status in [\"A\",\"B\"]",
            ));
        }
        if inv.kind == InvariantKind::Eventual && inv.window_ms.is_none() {
            findings.push(CrossInvariantLintFinding::error(
                "missing_window_ms",
                Some(&inv.name),
                "eventual invariants require window_ms",
            ));
        }
        if inv.kind == InvariantKind::Eventual && inv.window_ms == Some(0) {
            findings.push(CrossInvariantLintFinding::error(
                "invalid_window_ms",
                Some(&inv.name),
                "window_ms must be > 0 for eventual invariants",
            ));
        }
    }

    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cross_invariant::parse_cross_invariants;

    #[test]
    fn lint_reports_duplicates() {
        let src = r#"
[[invariant]]
name = "dup"
on = "Order.*"
assert = "related(Payment, payment_id).status in [\"Captured\"]"

[[invariant]]
name = "dup"
on = "Order.*"
assert = "related(Payment, payment_id).status in [\"Captured\"]"
"#;
        let spec = parse_cross_invariants(src).expect("should parse");
        let findings = lint_cross_invariants(&spec);
        assert!(
            findings
                .iter()
                .any(|f| f.code == "duplicate_invariant_name"),
            "{findings:?}"
        );
    }
}
