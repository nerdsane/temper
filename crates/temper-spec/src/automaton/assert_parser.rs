//! Shared assertion expression parser for IOA invariants.
//!
//! Parses `[[invariant]]` `assert` expressions into structured [`ParsedAssert`]
//! variants. Used by both `temper-verify` (model builder) and `temper-server`
//! (simulation handler) to ensure consistent classification.

use std::collections::BTreeSet;

/// A comparison operator for counter assertions.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    /// An assertion that is unconditionally true (`true`).
    Always,
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
    /// One counter must satisfy a comparison against another counter.
    CounterVarCompare {
        left: String,
        op: AssertCompareOp,
        right: String,
    },
    /// A declared string field must contain at least one character (`var != ''`).
    StringNonEmpty { var: String },
    /// A bare boolean variable must be true (e.g., `payment_captured`).
    ///
    /// `expect = true` matches `name`; `expect = false` matches `!name`.
    BoolRequired { var: String, expect: bool },
    /// All subexpressions must hold (short-circuit).
    And(Vec<ParsedAssert>),
    /// At least one subexpression must hold (short-circuit).
    Or(Vec<ParsedAssert>),
}

impl ParsedAssert {
    /// Return whether every typed reference is declared and the assertion
    /// shape is supported by the verification contract.
    ///
    /// Runtime-enforced leaves are supported on their own. Mixing them into a
    /// boolean compound is not yet atomic across all verification backends and
    /// therefore remains unsupported. Ordering assertions are likewise not in
    /// the verified subset.
    pub fn is_supported_safety_assertion(
        &self,
        bool_names: &BTreeSet<String>,
        counter_names: &BTreeSet<String>,
        string_names: &BTreeSet<String>,
        status_names: &BTreeSet<String>,
    ) -> bool {
        match self {
            Self::Always | Self::NoFurtherTransitions => true,
            Self::CounterPositive { var } | Self::CounterCompare { var, .. } => {
                counter_names.contains(var)
            }
            Self::CounterVarCompare { left, right, .. } => {
                counter_names.contains(left) && counter_names.contains(right)
            }
            Self::StringNonEmpty { var } => string_names.contains(var),
            Self::BoolRequired { var, .. } => bool_names.contains(var),
            Self::NeverState { state } => status_names.contains(state),
            Self::OrderingConstraint { .. } => false,
            Self::And(parts) | Self::Or(parts) => {
                !parts.iter().any(Self::contains_runtime_enforced_leaf)
                    && parts.iter().all(|part| {
                        part.is_supported_safety_assertion(
                            bool_names,
                            counter_names,
                            string_names,
                            status_names,
                        )
                    })
            }
        }
    }

    fn contains_runtime_enforced_leaf(&self) -> bool {
        match self {
            Self::CounterVarCompare { .. } | Self::StringNonEmpty { .. } => true,
            Self::And(parts) | Self::Or(parts) => {
                parts.iter().any(Self::contains_runtime_enforced_leaf)
            }
            _ => false,
        }
    }
}

/// Parse an assertion expression from an IOA spec into a [`ParsedAssert`].
///
/// Returns `None` for expressions that cannot be structurally parsed.
///
/// # Grammar
///
/// ```text
/// expr    = or_expr
/// or_expr = and_expr ( "||" and_expr )*
/// and_expr = atom ( "&&" atom )*
/// atom    = "(" expr ")" | "!" atom | terminal
/// terminal = counter_cmp | ordering(...) | never(...)
///          | true | no_further_transitions | is_true bool | bare_bool
/// ```
///
/// # Recognized terminals
///
/// - `"items > 0"` → `CounterPositive`
/// - `"no_further_transitions"` → `NoFurtherTransitions`
/// - `"ordering(StateA, StateB)"` → `OrderingConstraint`
/// - `"never(StateName)"` → `NeverState`
/// - `"var >= N"`, `"var <= N"`, `"var == N"`, `"var > N"`, `"var < N"` → `CounterCompare`
/// - `"flag"` / `"!flag"` → `BoolRequired`
/// - `"is_true flag"` → `BoolRequired`
/// - `"true"` → `Always`
///
/// # Compound expressions
///
/// - `"a && b && c"` → `And(vec![a, b, c])`
/// - `"a || b"` → `Or(vec![a, b])`
/// - `"a && (b || c)"` → `And(vec![a, Or(vec![b, c])])`
pub fn parse_assert_expr(expr: &str) -> Option<ParsedAssert> {
    if let Some(var) = expr.trim().strip_suffix("!= ''").map(str::trim)
        && is_identifier(var)
    {
        return Some(ParsedAssert::StringNonEmpty {
            var: var.to_string(),
        });
    }
    let tokens = tokenize(expr)?;
    let mut cursor = 0;
    let result = parse_or(&tokens, &mut cursor)?;
    // Reject trailing garbage.
    if cursor != tokens.len() {
        return None;
    }
    Some(result)
}

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    LParen,
    RParen,
    And,
    Or,
    Not,
    Comma,
    Ident(String),
    Number(String),
    CmpOp(AssertCompareOp),
}

fn tokenize(input: &str) -> Option<Vec<Tok>> {
    let bytes = input.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        match c {
            b'(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            b')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            b',' => {
                out.push(Tok::Comma);
                i += 1;
            }
            b'&' if i + 1 < bytes.len() && bytes[i + 1] == b'&' => {
                out.push(Tok::And);
                i += 2;
            }
            b'|' if i + 1 < bytes.len() && bytes[i + 1] == b'|' => {
                out.push(Tok::Or);
                i += 2;
            }
            b'!' if i + 1 < bytes.len() && bytes[i + 1] == b'=' => {
                // "!=" is not supported — reject.
                return None;
            }
            b'!' => {
                out.push(Tok::Not);
                i += 1;
            }
            b'>' if i + 1 < bytes.len() && bytes[i + 1] == b'=' => {
                out.push(Tok::CmpOp(AssertCompareOp::Gte));
                i += 2;
            }
            b'<' if i + 1 < bytes.len() && bytes[i + 1] == b'=' => {
                out.push(Tok::CmpOp(AssertCompareOp::Lte));
                i += 2;
            }
            b'=' if i + 1 < bytes.len() && bytes[i + 1] == b'=' => {
                out.push(Tok::CmpOp(AssertCompareOp::Eq));
                i += 2;
            }
            b'>' => {
                out.push(Tok::CmpOp(AssertCompareOp::Gt));
                i += 1;
            }
            b'<' => {
                out.push(Tok::CmpOp(AssertCompareOp::Lt));
                i += 1;
            }
            b if b.is_ascii_alphabetic() || b == b'_' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                out.push(Tok::Ident(input[start..i].to_string()));
            }
            b if b.is_ascii_digit() => {
                let start = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                out.push(Tok::Number(input[start..i].to_string()));
            }
            _ => return None,
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Recursive-descent parser
// ---------------------------------------------------------------------------

fn parse_or(tokens: &[Tok], cursor: &mut usize) -> Option<ParsedAssert> {
    let first = parse_and(tokens, cursor)?;
    let mut parts = vec![first];
    while matches!(tokens.get(*cursor), Some(Tok::Or)) {
        *cursor += 1;
        parts.push(parse_and(tokens, cursor)?);
    }
    if parts.len() == 1 {
        parts.pop()
    } else {
        Some(ParsedAssert::Or(parts))
    }
}

fn parse_and(tokens: &[Tok], cursor: &mut usize) -> Option<ParsedAssert> {
    let first = parse_atom(tokens, cursor)?;
    let mut parts = vec![first];
    while matches!(tokens.get(*cursor), Some(Tok::And)) {
        *cursor += 1;
        parts.push(parse_atom(tokens, cursor)?);
    }
    if parts.len() == 1 {
        parts.pop()
    } else {
        Some(ParsedAssert::And(parts))
    }
}

fn parse_atom(tokens: &[Tok], cursor: &mut usize) -> Option<ParsedAssert> {
    match tokens.get(*cursor) {
        Some(Tok::LParen) => {
            *cursor += 1;
            let inner = parse_or(tokens, cursor)?;
            if !matches!(tokens.get(*cursor), Some(Tok::RParen)) {
                return None;
            }
            *cursor += 1;
            Some(inner)
        }
        Some(Tok::Not) => {
            *cursor += 1;
            let inner = parse_atom(tokens, cursor)?;
            // Only `!bool_var` supported; negating compounds or comparisons needs
            // explicit De Morgan expansion by the spec author.
            if let ParsedAssert::BoolRequired { var, expect } = inner {
                Some(ParsedAssert::BoolRequired {
                    var,
                    expect: !expect,
                })
            } else {
                None
            }
        }
        _ => parse_terminal(tokens, cursor),
    }
}

fn parse_terminal(tokens: &[Tok], cursor: &mut usize) -> Option<ParsedAssert> {
    let start = *cursor;
    let first = tokens.get(*cursor)?.clone();

    // no_further_transitions (bare identifier).
    if let Tok::Ident(name) = &first {
        if name == "true" {
            *cursor += 1;
            return Some(ParsedAssert::Always);
        }

        if name == "is_true" {
            let Tok::Ident(var) = tokens.get(*cursor + 1)?.clone() else {
                return None;
            };
            *cursor += 2;
            return Some(ParsedAssert::BoolRequired { var, expect: true });
        }

        // ordering(A, B)
        if name == "ordering" && matches!(tokens.get(*cursor + 1), Some(Tok::LParen)) {
            *cursor += 2;
            let before = match tokens.get(*cursor) {
                Some(Tok::Ident(s)) => s.clone(),
                _ => return None,
            };
            *cursor += 1;
            if !matches!(tokens.get(*cursor), Some(Tok::Comma)) {
                return None;
            }
            *cursor += 1;
            let after = match tokens.get(*cursor) {
                Some(Tok::Ident(s)) => s.clone(),
                _ => return None,
            };
            *cursor += 1;
            if !matches!(tokens.get(*cursor), Some(Tok::RParen)) {
                return None;
            }
            *cursor += 1;
            return Some(ParsedAssert::OrderingConstraint { before, after });
        }

        // never(StateName)
        if name == "never" && matches!(tokens.get(*cursor + 1), Some(Tok::LParen)) {
            *cursor += 2;
            let state = match tokens.get(*cursor) {
                Some(Tok::Ident(s)) => s.clone(),
                _ => return None,
            };
            *cursor += 1;
            if !matches!(tokens.get(*cursor), Some(Tok::RParen)) {
                return None;
            }
            *cursor += 1;
            return Some(ParsedAssert::NeverState { state });
        }

        // Counter comparison: "var OP N".
        if let (Some(Tok::CmpOp(op)), Some(Tok::Number(n))) =
            (tokens.get(*cursor + 1), tokens.get(*cursor + 2))
        {
            let var = name.clone();
            let op = op.clone();
            let value: usize = n.parse().ok()?;
            *cursor += 3;
            // "var > 0" is CounterPositive.
            if matches!(op, AssertCompareOp::Gt) && value == 0 {
                return Some(ParsedAssert::CounterPositive { var });
            }
            return Some(ParsedAssert::CounterCompare { var, op, value });
        }
        if let (Some(Tok::CmpOp(op)), Some(Tok::Ident(right))) =
            (tokens.get(*cursor + 1), tokens.get(*cursor + 2))
        {
            *cursor += 3;
            return Some(ParsedAssert::CounterVarCompare {
                left: name.clone(),
                op: op.clone(),
                right: right.clone(),
            });
        }

        // no_further_transitions (exactly).
        if name == "no_further_transitions" {
            *cursor += 1;
            return Some(ParsedAssert::NoFurtherTransitions);
        }

        // Bare boolean identifier.
        *cursor += 1;
        return Some(ParsedAssert::BoolRequired {
            var: name.clone(),
            expect: true,
        });
    }

    // Reset cursor on failed match so the caller can try another path.
    *cursor = start;
    None
}

fn is_identifier(value: &str) -> bool {
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_production_boolean_aliases() {
        assert_eq!(parse_assert_expr("true"), Some(ParsedAssert::Always));
        assert_eq!(
            parse_assert_expr("is_true ready"),
            Some(ParsedAssert::BoolRequired {
                var: "ready".to_string(),
                expect: true,
            })
        );
    }

    #[test]
    fn parses_runtime_enforced_production_forms() {
        assert_eq!(
            parse_assert_expr("used_bytes <= quota_limit"),
            Some(ParsedAssert::CounterVarCompare {
                left: "used_bytes".to_string(),
                op: AssertCompareOp::Lte,
                right: "quota_limit".to_string(),
            })
        );
        assert_eq!(
            parse_assert_expr("goal != ''"),
            Some(ParsedAssert::StringNonEmpty {
                var: "goal".to_string(),
            })
        );
    }

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
    fn test_bare_bool_is_bool_required() {
        assert_eq!(
            parse_assert_expr("payment_captured"),
            Some(ParsedAssert::BoolRequired {
                var: "payment_captured".to_string(),
                expect: true,
            })
        );
    }

    #[test]
    fn test_negated_bool() {
        assert_eq!(
            parse_assert_expr("!payment_captured"),
            Some(ParsedAssert::BoolRequired {
                var: "payment_captured".to_string(),
                expect: false,
            })
        );
    }

    #[test]
    fn test_empty_returns_none() {
        assert_eq!(parse_assert_expr(""), None);
        assert_eq!(parse_assert_expr("   "), None);
    }

    #[test]
    fn test_unknown_symbols_return_none() {
        assert_eq!(parse_assert_expr("@@"), None);
        assert_eq!(parse_assert_expr("a != b"), None);
    }

    #[test]
    fn test_gt_zero_is_counter_positive_not_compare() {
        let result = parse_assert_expr("items > 0");
        assert!(matches!(result, Some(ParsedAssert::CounterPositive { .. })));
    }

    #[test]
    fn test_gte_zero_is_counter_compare() {
        assert_eq!(
            parse_assert_expr("items >= 0"),
            Some(ParsedAssert::CounterCompare {
                var: "items".to_string(),
                op: AssertCompareOp::Gte,
                value: 0,
            })
        );
    }

    // --- Compound expressions ---

    #[test]
    fn test_and_two_bools() {
        assert_eq!(
            parse_assert_expr("a && b"),
            Some(ParsedAssert::And(vec![
                ParsedAssert::BoolRequired {
                    var: "a".into(),
                    expect: true
                },
                ParsedAssert::BoolRequired {
                    var: "b".into(),
                    expect: true
                },
            ]))
        );
    }

    #[test]
    fn test_and_three_bools() {
        assert_eq!(
            parse_assert_expr("migrations_ok && typecheck_ok && unit_tests_ok"),
            Some(ParsedAssert::And(vec![
                ParsedAssert::BoolRequired {
                    var: "migrations_ok".into(),
                    expect: true
                },
                ParsedAssert::BoolRequired {
                    var: "typecheck_ok".into(),
                    expect: true
                },
                ParsedAssert::BoolRequired {
                    var: "unit_tests_ok".into(),
                    expect: true
                },
            ]))
        );
    }

    #[test]
    fn test_or_two_bools() {
        assert_eq!(
            parse_assert_expr("reviewed_by_code || reviewed_by_dst"),
            Some(ParsedAssert::Or(vec![
                ParsedAssert::BoolRequired {
                    var: "reviewed_by_code".into(),
                    expect: true
                },
                ParsedAssert::BoolRequired {
                    var: "reviewed_by_dst".into(),
                    expect: true
                },
            ]))
        );
    }

    #[test]
    fn test_paren_precedence() {
        // Without parens, && binds tighter than ||.
        // "a && (b || c)" = And(a, Or(b, c))
        assert_eq!(
            parse_assert_expr("a && (b || c)"),
            Some(ParsedAssert::And(vec![
                ParsedAssert::BoolRequired {
                    var: "a".into(),
                    expect: true
                },
                ParsedAssert::Or(vec![
                    ParsedAssert::BoolRequired {
                        var: "b".into(),
                        expect: true
                    },
                    ParsedAssert::BoolRequired {
                        var: "c".into(),
                        expect: true
                    },
                ]),
            ]))
        );
    }

    #[test]
    fn test_precedence_without_parens() {
        // "a || b && c" = Or(a, And(b, c)) — && binds tighter.
        assert_eq!(
            parse_assert_expr("a || b && c"),
            Some(ParsedAssert::Or(vec![
                ParsedAssert::BoolRequired {
                    var: "a".into(),
                    expect: true
                },
                ParsedAssert::And(vec![
                    ParsedAssert::BoolRequired {
                        var: "b".into(),
                        expect: true
                    },
                    ParsedAssert::BoolRequired {
                        var: "c".into(),
                        expect: true
                    },
                ]),
            ]))
        );
    }

    #[test]
    fn test_nested_parens() {
        assert_eq!(
            parse_assert_expr("((a))"),
            Some(ParsedAssert::BoolRequired {
                var: "a".into(),
                expect: true
            })
        );
    }

    #[test]
    fn test_mixed_compound_with_compare() {
        assert_eq!(
            parse_assert_expr("items > 0 && payment_captured"),
            Some(ParsedAssert::And(vec![
                ParsedAssert::CounterPositive {
                    var: "items".into()
                },
                ParsedAssert::BoolRequired {
                    var: "payment_captured".into(),
                    expect: true
                },
            ]))
        );
    }

    #[test]
    fn test_unbalanced_paren_returns_none() {
        assert_eq!(parse_assert_expr("(a && b"), None);
        assert_eq!(parse_assert_expr("a && b)"), None);
    }

    #[test]
    fn test_whitespace_insensitive_compound() {
        assert_eq!(
            parse_assert_expr("  a   &&   b  "),
            Some(ParsedAssert::And(vec![
                ParsedAssert::BoolRequired {
                    var: "a".into(),
                    expect: true
                },
                ParsedAssert::BoolRequired {
                    var: "b".into(),
                    expect: true
                },
            ]))
        );
    }

    #[test]
    fn test_negated_in_compound() {
        assert_eq!(
            parse_assert_expr("a && !b"),
            Some(ParsedAssert::And(vec![
                ParsedAssert::BoolRequired {
                    var: "a".into(),
                    expect: true
                },
                ParsedAssert::BoolRequired {
                    var: "b".into(),
                    expect: false
                },
            ]))
        );
    }

    #[test]
    fn test_trailing_operator_returns_none() {
        assert_eq!(parse_assert_expr("a &&"), None);
        assert_eq!(parse_assert_expr("&& a"), None);
    }

    #[test]
    fn safety_support_contract_validates_declarations_and_shape() {
        let bools = BTreeSet::from(["ready".to_string()]);
        let counters = BTreeSet::from(["items".to_string()]);
        let strings = BTreeSet::from(["goal".to_string()]);
        let statuses = BTreeSet::from(["Draft".to_string()]);

        assert!(
            parse_assert_expr("items > 0")
                .unwrap()
                .is_supported_safety_assertion(&bools, &counters, &strings, &statuses)
        );
        for unsupported in ["ghost > 0", "never(Ghost)", "ordering(Draft, Ghost)"] {
            assert!(
                !parse_assert_expr(unsupported)
                    .unwrap()
                    .is_supported_safety_assertion(&bools, &counters, &strings, &statuses),
                "{unsupported} must remain outside the supported safety contract"
            );
        }
        let compound_runtime = ParsedAssert::And(vec![
            ParsedAssert::StringNonEmpty { var: "goal".into() },
            ParsedAssert::BoolRequired {
                var: "ready".into(),
                expect: true,
            },
        ]);
        assert!(
            !compound_runtime.is_supported_safety_assertion(&bools, &counters, &strings, &statuses)
        );
    }
}
