use super::*;

const FILTER_TEST_STACK_BYTES: usize = 512 * 1024;

fn assert_on_small_stack(name: &str, test: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .name(name.into())
        .stack_size(FILTER_TEST_STACK_BYTES)
        .spawn(test)
        .expect("spawn constrained-stack filter test")
        .join()
        .expect("filter boundary must fit the constrained request stack");
}

// -- Security regression tests (ARN-176) ---------------------------------
//
// Denial-of-service regressions on the public OData `$filter` surface. Each
// hostile input now returns `InvalidFilter` (surfaced as HTTP 400) quickly
// instead of hanging, exhausting memory, or crashing the request thread.

#[test]
fn filter_dollar_sign_returns_error_not_hang() {
    // Pre-fix: the tokenizer accepted '$' as an identifier-start character,
    // but the identifier loop never consumed it, so `i` never advanced — an
    // infinite loop that also grew the token vec without bound, hanging (and
    // eventually OOM-ing) the request thread. The watchdog below turns that
    // hang into a test failure instead of blocking the suite forever. On a
    // regression the spawned thread is intentionally left detached (it never
    // returns); the harness process exits and reaps it after the failure.
    use std::sync::mpsc;
    use std::time::Duration;

    let (tx, rx) = mpsc::channel();
    // A stray '$' outside a string literal is the trigger. Inside quotes it
    // is fine (handled by the string-literal branch).
    std::thread::spawn(move || {
        let _ = tx.send(parse_filter("Name eq $foo").is_err());
    });

    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(is_err) => assert!(
            is_err,
            "'$' in a filter value must return InvalidFilter, not succeed"
        ),
        Err(_) => {
            panic!("parse_filter hung on '$' — tokenizer infinite loop is not fixed")
        }
    }
}

#[test]
fn filter_bare_dollar_sign_is_rejected() {
    // A lone '$' is an unexpected character, not an identifier.
    assert!(parse_filter("$").is_err());
}

#[test]
fn filter_deeply_nested_parens_returns_error_not_overflow() {
    // Pre-fix: unbounded recursion (parse_primary -> parse_or per '(')
    // overflowed the request thread's stack and aborted the process. Now
    // bounded: returns InvalidFilter once nesting passes FILTER_DEPTH_BUDGET.
    let depth = 100_000;
    let mut input = String::with_capacity(depth * 2 + 16);
    for _ in 0..depth {
        input.push('(');
    }
    input.push_str("Name eq 'x'");
    for _ in 0..depth {
        input.push(')');
    }
    assert!(
        parse_filter(&input).is_err(),
        "deeply nested parens must return InvalidFilter, not overflow the stack"
    );
}

#[test]
fn filter_deeply_nested_not_returns_error_not_overflow() {
    // Pre-fix: `not not not …` recursed parse_not without bound and
    // overflowed the stack. Now bounded past FILTER_DEPTH_BUDGET.
    let mut input = String::new();
    for _ in 0..100_000 {
        input.push_str("not ");
    }
    input.push_str("Active eq true");
    assert!(
        parse_filter(&input).is_err(),
        "deeply nested 'not' must return InvalidFilter, not overflow the stack"
    );
}

#[test]
fn filter_deeply_nested_functions_returns_error_not_overflow() {
    // Pre-fix: `f(f(f(…)))` recursed parse_argument_list -> parse_or without
    // bound and overflowed the stack. Now bounded past FILTER_DEPTH_BUDGET.
    let depth = 100_000;
    let mut input = String::with_capacity(depth * 2 + 8);
    for _ in 0..depth {
        input.push_str("f(");
    }
    input.push('1');
    for _ in 0..depth {
        input.push(')');
    }
    assert!(
        parse_filter(&input).is_err(),
        "deeply nested function calls must return InvalidFilter, not overflow"
    );
}

#[test]
fn filter_moderately_nested_parens_still_parse() {
    // Nesting well within FILTER_DEPTH_BUDGET must still parse — the depth
    // bound must not reject legitimate queries.
    let depth = FILTER_DEPTH_BUDGET / 2;
    let mut input = String::new();
    for _ in 0..depth {
        input.push('(');
    }
    input.push_str("Name eq 'x'");
    for _ in 0..depth {
        input.push(')');
    }
    assert!(
        parse_filter(&input).is_ok(),
        "nesting within the depth bound must still parse"
    );

    // A useful wide, shallow filter must fit the total parse budget; width
    // is bounded independently from recursive depth.
    let wide = (0..500)
        .map(|n| format!("Id eq {n}"))
        .collect::<Vec<_>>()
        .join(" or ");
    assert!(
        parse_filter(&wide).is_ok(),
        "moderately wide filters must remain supported"
    );
}

#[test]
fn filter_input_byte_budget_is_inclusive() {
    let at_budget = "a".repeat(FILTER_INPUT_BYTE_BUDGET);
    assert!(parse_filter(&at_budget).is_ok());

    let over_budget = "a".repeat(FILTER_INPUT_BYTE_BUDGET + 1);
    assert!(parse_filter(&over_budget).is_err());
}

#[test]
fn filter_token_budget_is_inclusive() {
    let at_budget = std::iter::repeat_n("a", FILTER_TOKEN_BUDGET)
        .collect::<Vec<_>>()
        .join(" ");
    let mut budget = FilterBudget::new(&at_budget).unwrap();
    assert_eq!(
        tokenize_filter(&at_budget, &mut budget).unwrap().len(),
        FILTER_TOKEN_BUDGET
    );

    let over_budget = format!("{at_budget} a");
    let mut budget = FilterBudget::new(&over_budget).unwrap();
    assert!(tokenize_filter(&over_budget, &mut budget).is_err());
}

#[test]
fn filter_node_budget_is_inclusive() {
    let mut budget = FilterBudget::new("").unwrap();
    for _ in 0..FILTER_NODE_BUDGET {
        assert!(budget.consume_node(0).is_ok());
    }
    assert!(budget.consume_node(0).is_err());
}

#[test]
fn filter_literal_byte_budget_is_inclusive() {
    let at_budget = format!("Name eq '{}'", "x".repeat(FILTER_LITERAL_BYTE_BUDGET));
    assert!(parse_filter(&at_budget).is_ok());

    let over_budget = format!("Name eq '{}'", "x".repeat(FILTER_LITERAL_BYTE_BUDGET + 1));
    assert!(parse_filter(&over_budget).is_err());
}

#[test]
fn filter_function_argument_budget_is_inclusive() {
    let at_budget = format!(
        "f({})",
        std::iter::repeat_n("a", FILTER_ARGUMENT_BUDGET)
            .collect::<Vec<_>>()
            .join(",")
    );
    assert!(parse_filter(&at_budget).is_ok());

    let over_budget = format!(
        "f({})",
        std::iter::repeat_n("a", FILTER_ARGUMENT_BUDGET + 1)
            .collect::<Vec<_>>()
            .join(",")
    );
    assert!(parse_filter(&over_budget).is_err());
}

#[test]
fn filter_operator_budget_is_inclusive() {
    assert_eq!(FILTER_OPERATOR_BUDGET % 2, 0);
    let comparisons = (0..FILTER_OPERATOR_BUDGET / 2)
        .map(|n| format!("Id eq {n}"))
        .collect::<Vec<_>>()
        .join(" or ");
    assert!(parse_filter(&format!("not {comparisons}")).is_ok());
    assert!(parse_filter(&format!("not not {comparisons}")).is_err());
}

#[test]
fn filter_budgeted_wide_ast_parses_and_drops_on_small_stack() {
    let input = (0..FILTER_OPERATOR_BUDGET / 2)
        .map(|n| format!("Id eq {n}"))
        .collect::<Vec<_>>()
        .join(" or ");

    assert_on_small_stack("filter-wide-boundary", move || {
        let expr = parse_filter(&input).expect("expression must fit the budget");
        drop(expr);
    });
}

#[test]
fn filter_mixed_depth_and_width_fits_small_stack() {
    let wide = (0..FILTER_OPERATOR_BUDGET / 2)
        .map(|n| format!("Id eq {n}"))
        .collect::<Vec<_>>()
        .join(" or ");
    let input = format!(
        "{}{}{}",
        "(".repeat(FILTER_DEPTH_BUDGET),
        wide,
        ")".repeat(FILTER_DEPTH_BUDGET)
    );

    assert_on_small_stack("filter-mixed-budget-boundary", move || {
        let expr = parse_filter(&input).expect("mixed boundary expression must parse");
        drop(expr);
    });
}

#[test]
fn filter_wide_ast_over_budget_returns_error() {
    let input = (0..40_000)
        .map(|n| format!("Id eq {n}"))
        .collect::<Vec<_>>()
        .join(" or ");
    assert!(parse_filter(&input).is_err());
}

#[test]
fn filter_unterminated_string_returns_error() {
    // Pre-fix: an unterminated literal was silently accepted as if closed.
    assert!(parse_filter("Name eq 'foo").is_err());
}

#[test]
fn filter_depth_bound_is_inclusive_at_the_limit() {
    // Exercise the exact boundary so an off-by-one (e.g. flipping `>` to
    // `>=`) is caught: FILTER_DEPTH_BUDGET levels of nesting must parse, and one
    // level deeper must be rejected.
    let nest = |levels: usize| {
        let mut s = String::with_capacity(levels * 2 + 16);
        for _ in 0..levels {
            s.push('(');
        }
        s.push_str("Name eq 'x'");
        for _ in 0..levels {
            s.push(')');
        }
        s
    };
    assert_on_small_stack("filter-depth-boundary", move || {
        assert!(
            parse_filter(&nest(FILTER_DEPTH_BUDGET)).is_ok(),
            "exactly FILTER_DEPTH_BUDGET levels must be accepted"
        );
        assert!(
            parse_filter(&nest(FILTER_DEPTH_BUDGET + 1)).is_err(),
            "one level past FILTER_DEPTH_BUDGET must be rejected"
        );
    });
}
