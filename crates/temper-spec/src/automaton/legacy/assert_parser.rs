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
    /// A bare boolean variable must be true (e.g., `payment_captured`).
    ///
    /// `expect = true` matches `name`; `expect = false` matches `!name`.
    BoolRequired { var: String, expect: bool },
    /// All subexpressions must hold (short-circuit).
    And(Vec<ParsedAssert>),
    /// At least one subexpression must hold (short-circuit).
    Or(Vec<ParsedAssert>),
}

/// Why [`parse_assert_expr`] produced no [`ParsedAssert`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssertRejection {
    /// Not a shape this parser recognizes; another legacy syntax may still apply.
    Unrecognized,
    /// `(` or `!` nested past the nesting budget. No legacy syntax reads such text,
    /// so it goes to the predicate parser, which has the same budget.
    NestedTooDeep,
}

/// Parse an assertion expression from an IOA spec into a [`ParsedAssert`].
///
/// Returns [`AssertRejection::Unrecognized`] for expressions that cannot be
/// structurally parsed and [`AssertRejection::NestedTooDeep`] for text nested
/// past the budget.
///
/// # Grammar
///
/// ```text
/// expr    = or_expr
/// or_expr = and_expr ( "||" and_expr )*
/// and_expr = atom ( "&&" atom )*
/// atom    = "(" expr ")" | "!" atom | terminal
/// terminal = counter_cmp | ordering(...) | never(...)
///          | no_further_transitions | bare_bool
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
///
/// # Compound expressions
///
/// - `"a && b && c"` → `And(vec![a, b, c])`
/// - `"a || b"` → `Or(vec![a, b])`
/// - `"a && (b || c)"` → `And(vec![a, Or(vec![b, c])])`
pub fn parse_assert_expr(expr: &str) -> Result<ParsedAssert, AssertRejection> {
    let tokens = tokenize(expr).ok_or(AssertRejection::Unrecognized)?;
    let mut cursor = 0;
    let result = parse_or(&tokens, &mut cursor, 0)?;
    // Reject trailing garbage.
    if cursor != tokens.len() {
        return Err(AssertRejection::Unrecognized);
    }
    Ok(result)
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

/// Nesting budget for `(` and `!`, shared with the predicate parser. Each level
/// is a stack frame, and assert text comes from tenant-loaded specs, so parsing
/// stops at the budget with [`AssertRejection::NestedTooDeep`] and stays within
/// a bounded stack.
const MAX_DEPTH: usize = crate::predicate::MAX_DEPTH;

type Parsed = Result<ParsedAssert, AssertRejection>;

fn parse_or(tokens: &[Tok], cursor: &mut usize, depth: usize) -> Parsed {
    let first = parse_and(tokens, cursor, depth)?;
    let mut parts = vec![first];
    while matches!(tokens.get(*cursor), Some(Tok::Or)) {
        *cursor += 1;
        parts.push(parse_and(tokens, cursor, depth)?);
    }
    Ok(if parts.len() == 1 {
        parts.remove(0)
    } else {
        ParsedAssert::Or(parts)
    })
}

fn parse_and(tokens: &[Tok], cursor: &mut usize, depth: usize) -> Parsed {
    let first = parse_atom(tokens, cursor, depth)?;
    let mut parts = vec![first];
    while matches!(tokens.get(*cursor), Some(Tok::And)) {
        *cursor += 1;
        parts.push(parse_atom(tokens, cursor, depth)?);
    }
    Ok(if parts.len() == 1 {
        parts.remove(0)
    } else {
        ParsedAssert::And(parts)
    })
}

fn parse_atom(tokens: &[Tok], cursor: &mut usize, depth: usize) -> Parsed {
    let nested = matches!(tokens.get(*cursor), Some(Tok::LParen | Tok::Not));
    if nested && depth >= MAX_DEPTH {
        return Err(AssertRejection::NestedTooDeep);
    }
    match tokens.get(*cursor) {
        Some(Tok::LParen) => {
            *cursor += 1;
            let inner = parse_or(tokens, cursor, depth + 1)?;
            if !matches!(tokens.get(*cursor), Some(Tok::RParen)) {
                return Err(AssertRejection::Unrecognized);
            }
            *cursor += 1;
            Ok(inner)
        }
        Some(Tok::Not) => {
            *cursor += 1;
            let inner = parse_atom(tokens, cursor, depth + 1)?;
            // Only `!bool_var` supported; negating compounds or comparisons needs
            // explicit De Morgan expansion by the spec author.
            if let ParsedAssert::BoolRequired { var, expect } = inner {
                Ok(ParsedAssert::BoolRequired {
                    var,
                    expect: !expect,
                })
            } else {
                Err(AssertRejection::Unrecognized)
            }
        }
        _ => parse_terminal(tokens, cursor).ok_or(AssertRejection::Unrecognized),
    }
}

fn parse_terminal(tokens: &[Tok], cursor: &mut usize) -> Option<ParsedAssert> {
    let start = *cursor;
    let first = tokens.get(*cursor)?.clone();

    // no_further_transitions (bare identifier).
    if let Tok::Ident(name) = &first {
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

        // Counter comparison: "var OP N"
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_counter_positive() {
        assert_eq!(
            parse_assert_expr("items > 0").ok(),
            Some(ParsedAssert::CounterPositive {
                var: "items".to_string()
            })
        );
        assert_eq!(
            parse_assert_expr("  count > 0  ").ok(),
            Some(ParsedAssert::CounterPositive {
                var: "count".to_string()
            })
        );
    }

    #[test]
    fn test_no_further_transitions() {
        assert_eq!(
            parse_assert_expr("no_further_transitions").ok(),
            Some(ParsedAssert::NoFurtherTransitions)
        );
    }

    #[test]
    fn test_ordering_constraint() {
        assert_eq!(
            parse_assert_expr("ordering(Submitted, Shipped)").ok(),
            Some(ParsedAssert::OrderingConstraint {
                before: "Submitted".to_string(),
                after: "Shipped".to_string(),
            })
        );
    }

    #[test]
    fn test_never_state() {
        assert_eq!(
            parse_assert_expr("never(Invalid)").ok(),
            Some(ParsedAssert::NeverState {
                state: "Invalid".to_string()
            })
        );
    }

    #[test]
    fn test_counter_compare_gte() {
        assert_eq!(
            parse_assert_expr("items >= 1").ok(),
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
            parse_assert_expr("retries <= 5").ok(),
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
            parse_assert_expr("count == 3").ok(),
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
            parse_assert_expr("items > 5").ok(),
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
            parse_assert_expr("retries < 10").ok(),
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
            parse_assert_expr("payment_captured").ok(),
            Some(ParsedAssert::BoolRequired {
                var: "payment_captured".to_string(),
                expect: true,
            })
        );
    }

    #[test]
    fn test_negated_bool() {
        assert_eq!(
            parse_assert_expr("!payment_captured").ok(),
            Some(ParsedAssert::BoolRequired {
                var: "payment_captured".to_string(),
                expect: false,
            })
        );
    }

    #[test]
    fn test_empty_returns_none() {
        assert_eq!(parse_assert_expr("").ok(), None);
        assert_eq!(parse_assert_expr("   ").ok(), None);
    }

    #[test]
    fn test_unknown_symbols_return_none() {
        assert_eq!(parse_assert_expr("@@").ok(), None);
        assert_eq!(parse_assert_expr("a != b").ok(), None);
    }

    #[test]
    fn test_gt_zero_is_counter_positive_not_compare() {
        let result = parse_assert_expr("items > 0").ok();
        assert!(matches!(result, Some(ParsedAssert::CounterPositive { .. })));
    }

    #[test]
    fn test_gte_zero_is_counter_compare() {
        assert_eq!(
            parse_assert_expr("items >= 0").ok(),
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
            parse_assert_expr("a && b").ok(),
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
            parse_assert_expr("migrations_ok && typecheck_ok && unit_tests_ok").ok(),
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
            parse_assert_expr("reviewed_by_code || reviewed_by_dst").ok(),
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
            parse_assert_expr("a && (b || c)").ok(),
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
            parse_assert_expr("a || b && c").ok(),
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
            parse_assert_expr("((a))").ok(),
            Some(ParsedAssert::BoolRequired {
                var: "a".into(),
                expect: true
            })
        );
    }

    #[test]
    fn test_mixed_compound_with_compare() {
        assert_eq!(
            parse_assert_expr("items > 0 && payment_captured").ok(),
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
        assert_eq!(parse_assert_expr("(a && b").ok(), None);
        assert_eq!(parse_assert_expr("a && b)").ok(), None);
    }

    #[test]
    fn test_whitespace_insensitive_compound() {
        assert_eq!(
            parse_assert_expr("  a   &&   b  ").ok(),
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
            parse_assert_expr("a && !b").ok(),
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
        assert_eq!(parse_assert_expr("a &&").ok(), None);
        assert_eq!(parse_assert_expr("&& a").ok(), None);
    }

    #[test]
    fn test_nesting_within_budget_parses() {
        let parens = format!("{}a{}", "(".repeat(MAX_DEPTH), ")".repeat(MAX_DEPTH));
        assert_eq!(
            parse_assert_expr(&parens).ok(),
            Some(ParsedAssert::BoolRequired {
                var: "a".into(),
                expect: true
            })
        );
        let nots = format!("{}a", "!".repeat(MAX_DEPTH));
        assert!(parse_assert_expr(&nots).ok().is_some());
    }

    #[test]
    fn test_nesting_past_budget_is_nested_too_deep() {
        let over = MAX_DEPTH + 1;
        let cases = [
            format!("{}a{}", "(".repeat(over), ")".repeat(over)),
            format!("{}a", "!".repeat(over)),
            "!(".repeat(over) + "a" + &")".repeat(over),
            format!("b && {}a", "!".repeat(over)),
        ];
        for case in &cases {
            assert_eq!(
                parse_assert_expr(case),
                Err(AssertRejection::NestedTooDeep),
                "{case}"
            );
        }
    }

    /// Depth far past any stack: the parser stops at the budget instead of
    /// recursing once per level.
    #[test]
    fn test_very_deep_nesting_is_nested_too_deep() {
        let depth = 200_000;
        let parens = format!("{}a{}", "(".repeat(depth), ")".repeat(depth));
        assert_eq!(
            parse_assert_expr(&parens),
            Err(AssertRejection::NestedTooDeep)
        );
        let mixed = "!(".repeat(depth) + "a" + &")".repeat(depth);
        assert_eq!(
            parse_assert_expr(&mixed),
            Err(AssertRejection::NestedTooDeep)
        );
    }
}
