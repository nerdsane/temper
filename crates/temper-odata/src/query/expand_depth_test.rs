use super::*;

fn nested_expand(levels: usize) -> String {
    let mut query = String::from("$expand=");
    for level in 1..levels {
        query.push_str(&format!("Level{level}($expand="));
    }
    query.push_str("Leaf");
    query.push_str(&")".repeat(levels - 1));
    query
}

#[test]
fn an_expand_at_the_bound_parses_every_level() {
    let opts = parse_query_options(&nested_expand(MAX_EXPAND_DEPTH)).expect("parse");
    let mut items = opts.expand.expect("top-level $expand");
    for _ in 1..MAX_EXPAND_DEPTH {
        let nested = items[0].options.as_ref().and_then(|o| o.expand.clone());
        items = nested.expect("every level up to the bound is kept");
    }
    assert_eq!(items[0].property, "Leaf");
    assert!(items[0].options.is_none());
}

#[test]
fn an_expand_past_the_bound_is_rejected_by_name() {
    // The read path used to drop the fourth level without a word.
    let err = parse_query_options(&nested_expand(MAX_EXPAND_DEPTH + 1))
        .expect_err("a deeper $expand is an error");
    assert!(
        matches!(&err, ODataError::InvalidQueryOption { option, message }
            if option == "$expand" && message.contains(&format!("more than {MAX_EXPAND_DEPTH} levels"))),
        "{err}"
    );
}

#[test]
fn a_deeply_nested_expand_is_rejected_without_deep_recursion() {
    // The parser stops at the bound instead of recursing once per level, so
    // a crafted query cannot overflow a small request stack.
    let query = nested_expand(50_000);
    let handle = std::thread::Builder::new()
        .stack_size(512 * 1024)
        .spawn(move || parse_query_options(&query).is_err())
        .expect("spawn constrained-stack parser");
    assert!(handle.join().expect("parser did not overflow its stack"));
}
