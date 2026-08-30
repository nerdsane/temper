//! Semantic checks for parsed field invariants.

use std::collections::BTreeSet;

use crate::automaton::{Automaton, FieldInvariant, FieldPredicate};

use super::LintFinding;

/// Validate parsed `[[field_invariant]]` entries after structural parsing.
pub(super) fn lint_field_invariants(automaton: &Automaton, findings: &mut Vec<LintFinding>) {
    let mut seen_names: BTreeSet<&str> = BTreeSet::new();
    for invariant in &automaton.field_invariants {
        if invariant.name.trim().is_empty() {
            findings.push(LintFinding::error(
                "field_invariant_missing_name",
                "field_invariant has empty `name` — error responses would have no identifier",
            ));
        } else if !seen_names.insert(invariant.name.as_str()) {
            findings.push(LintFinding::error(
                "field_invariant_duplicate_name",
                format!(
                    "field_invariant '{}' is declared more than once",
                    invariant.name
                ),
            ));
        }

        if invariant.when.has_empty_combinator() {
            findings.push(LintFinding::error(
                "field_invariant_empty_combinator",
                format!(
                    "field_invariant '{}' `when` tree contains an empty `any_of`/`all_of` — rule is always inert or always fires",
                    invariant.name
                ),
            ));
        }
        if invariant.require.has_empty_combinator() {
            findings.push(LintFinding::error(
                "field_invariant_empty_combinator",
                format!(
                    "field_invariant '{}' `require` tree contains an empty `any_of`/`all_of` — rule is trivially true or trivially false",
                    invariant.name
                ),
            ));
        }

        for referenced in invariant.referenced_fields() {
            if !is_valid_field_identifier(&referenced) {
                findings.push(LintFinding::error(
                    "field_invariant_bad_field_name",
                    format!(
                        "field_invariant '{}' references field '{}' which is not a valid identifier",
                        invariant.name, referenced
                    ),
                ));
            }
        }

        check_unsatisfiable_same_field_equals(invariant, findings);
    }
}

fn check_unsatisfiable_same_field_equals(
    invariant: &FieldInvariant,
    findings: &mut Vec<LintFinding>,
) {
    if let (
        FieldPredicate::Equals {
            field: left_field,
            equals: left_value,
        },
        FieldPredicate::Equals {
            field: right_field,
            equals: right_value,
        },
    ) = (&invariant.when, &invariant.require)
        && left_field == right_field
        && left_value != right_value
    {
        findings.push(LintFinding::warning(
            "field_invariant_trivially_unsatisfiable",
            format!(
                "field_invariant '{}' requires field '{}' to equal both '{}' and '{}'",
                invariant.name, left_field, left_value, right_value
            ),
        ));
    }
}

fn is_valid_field_identifier(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    let mut chars = value.chars();
    let first = chars.next().unwrap(); // ci-ok: non-empty checked above
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|character| character.is_ascii_alphanumeric() || character == '_')
}
