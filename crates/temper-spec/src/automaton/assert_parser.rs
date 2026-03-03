//! Shared assertion expression parser for IOA invariants.
//!
//! Parses `[[invariant]]` `assert` expressions into structured [`ParsedAssert`]
//! variants. Used by both `temper-verify` (model builder) and `temper-server`
//! (simulation handler) to ensure consistent classification.

/// A comparison operator for counter assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssertCompareOp {
    /// Greater than.
    Gt,
    /// Greater than or equal.
    Gte,
    /// Less than.
    Lt,
    /// Less than or equal.
    Lte,
    /// Equal.
    Eq,
}

/// A parsed assertion expression from an IOA `[[invariant]]` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedAssert {
    /// A counter variable must be positive (`var > 0`).
    CounterPositive { var: String },
    /// The entity is in a terminal state — no further transitions allowed.
    NoFurtherTransitions,
    /// State A must have been visited before state B in event history.
    /// Expressed as: `ordering(A, B)`.
    OrderingConstraint { before: String, after: String },
    /// The entity should never be in this state.
    /// Expressed as: `never(StateName)`.
    NeverState { state: String },
    /// A counter must satisfy a comparison (e.g., `items >= 1`, `retries < 5`).
    CounterCompare {
        var: String,
        op: AssertCompareOp,
        value: usize,
    },
}

/// Parse an assertion expression from an IOA spec into a [`ParsedAssert`].
///
/// Returns `None` for expressions that cannot be structurally parsed.
///
/// # Recognized patterns
///
/// - `"items > 0"` → `CounterPositive { var: "items" }`
/// - `"no_further_transitions"` → `NoFurtherTransitions`
/// - `"ordering(StateA, StateB)"` → `OrderingConstraint { before, after }`
/// - `"never(StateName)"` → `NeverState { state }`
/// - `"var >= N"`, `"var <= N"`, `"var == N"`, `"var > N"`, `"var < N"` → `CounterCompare`
pub fn parse_assert_expr(expr: &str) -> Option<ParsedAssert> {
    let trimmed = expr.trim();

    // Pattern: "items > 0" or "var > 0" — shorthand for CounterPositive.
    if trimmed.contains("> 0") && !trimmed.contains(">=") {
        let var = trimmed.split('>').next()?.trim().to_string();
        if !var.is_empty() {
            return Some(ParsedAssert::CounterPositive { var });
        }
    }

    // Pattern: "no_further_transitions"
    if trimmed == "no_further_transitions" {
        return Some(ParsedAssert::NoFurtherTransitions);
    }

    // Pattern: "ordering(StateA, StateB)" — StateA must precede StateB.
    if trimmed.starts_with("ordering(") && trimmed.ends_with(')') {
        let inner = &trimmed[9..trimmed.len() - 1];
        let parts: Vec<&str> = inner.split(',').map(|s| s.trim()).collect();
        if parts.len() == 2 && !parts[0].is_empty() && !parts[1].is_empty() {
            return Some(ParsedAssert::OrderingConstraint {
                before: parts[0].to_string(),
                after: parts[1].to_string(),
            });
        }
    }

    // Pattern: "never(StateName)" — entity should never be in this state.
    if trimmed.starts_with("never(") && trimmed.ends_with(')') {
        let state = trimmed[6..trimmed.len() - 1].trim().to_string();
        if !state.is_empty() {
            return Some(ParsedAssert::NeverState { state });
        }
    }

    // Generalized counter comparison: "var >= N", "var <= N", "var == N",
    // "var > N", "var < N". Order matters: check two-char ops before one-char.
    let ops: &[(&str, AssertCompareOp)] = &[
        (">=", AssertCompareOp::Gte),
        ("<=", AssertCompareOp::Lte),
        ("==", AssertCompareOp::Eq),
        (">", AssertCompareOp::Gt),
        ("<", AssertCompareOp::Lt),
    ];
    for (op_str, op) in ops {
        if let Some(pos) = trimmed.find(op_str) {
            let var = trimmed[..pos].trim().to_string();
            let val_str = trimmed[pos + op_str.len()..].trim();
            if let Ok(value) = val_str.parse::<usize>() {
                // "var > 0" is already handled by CounterPositive above.
                if *op_str == ">" && value == 0 {
                    continue;
                }
                if !var.is_empty() {
                    return Some(ParsedAssert::CounterCompare {
                        var,
                        op: op.clone(),
                        value,
                    });
                }
            }
        }
    }

    // Unrecognized expression.
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_counter_positive() {
        assert_eq!(
            parse_assert_expr("items > 0"),
            Some(ParsedAssert::CounterPositive {
                var: "items".to_string()
            })
        );
        assert_eq!(
            parse_assert_expr("  count > 0  "),
            Some(ParsedAssert::CounterPositive {
                var: "count".to_string()
            })
        );
    }

    #[test]
    fn test_no_further_transitions() {
        assert_eq!(
            parse_assert_expr("no_further_transitions"),
            Some(ParsedAssert::NoFurtherTransitions)
        );
    }

    #[test]
    fn test_ordering_constraint() {
        assert_eq!(
            parse_assert_expr("ordering(Submitted, Shipped)"),
            Some(ParsedAssert::OrderingConstraint {
                before: "Submitted".to_string(),
                after: "Shipped".to_string(),
            })
        );
    }

    #[test]
    fn test_never_state() {
        assert_eq!(
            parse_assert_expr("never(Invalid)"),
            Some(ParsedAssert::NeverState {
                state: "Invalid".to_string()
            })
        );
    }

    #[test]
    fn test_counter_compare_gte() {
        assert_eq!(
            parse_assert_expr("items >= 1"),
            Some(ParsedAssert::CounterCompare {
                var: "items".to_string(),
                op: AssertCompareOp::Gte,
                value: 1,
            })
        );
    }

    #[test]
    fn test_counter_compare_lte() {
        assert_eq!(
            parse_assert_expr("retries <= 5"),
            Some(ParsedAssert::CounterCompare {
                var: "retries".to_string(),
                op: AssertCompareOp::Lte,
                value: 5,
            })
        );
    }

    #[test]
    fn test_counter_compare_eq() {
        assert_eq!(
            parse_assert_expr("count == 3"),
            Some(ParsedAssert::CounterCompare {
                var: "count".to_string(),
                op: AssertCompareOp::Eq,
                value: 3,
            })
        );
    }

    #[test]
    fn test_counter_compare_gt() {
        assert_eq!(
            parse_assert_expr("items > 5"),
            Some(ParsedAssert::CounterCompare {
                var: "items".to_string(),
                op: AssertCompareOp::Gt,
                value: 5,
            })
        );
    }

    #[test]
    fn test_counter_compare_lt() {
        assert_eq!(
            parse_assert_expr("retries < 10"),
            Some(ParsedAssert::CounterCompare {
                var: "retries".to_string(),
                op: AssertCompareOp::Lt,
                value: 10,
            })
        );
    }

    #[test]
    fn test_unrecognized_returns_none() {
        assert_eq!(parse_assert_expr("payment_captured"), None);
        assert_eq!(parse_assert_expr("some random text"), None);
        assert_eq!(parse_assert_expr(""), None);
    }

    #[test]
    fn test_gt_zero_is_counter_positive_not_compare() {
        // "var > 0" should be CounterPositive, not CounterCompare
        let result = parse_assert_expr("items > 0");
        assert!(matches!(result, Some(ParsedAssert::CounterPositive { .. })));
    }

    #[test]
    fn test_gte_zero_is_counter_compare() {
        // "var >= 0" is NOT counter positive, it's a (trivial) compare
        assert_eq!(
            parse_assert_expr("items >= 0"),
            Some(ParsedAssert::CounterCompare {
                var: "items".to_string(),
                op: AssertCompareOp::Gte,
                value: 0,
            })
        );
    }
}
