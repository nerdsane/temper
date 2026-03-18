use super::*;

// ── parse_kv ──────────────────────────────────────────────

#[test]
fn parse_kv_simple() {
    let (key, val) = parse_kv("name = \"Order\"").unwrap();
    assert_eq!(key, "name");
    assert_eq!(val, "Order");
}

#[test]
fn parse_kv_no_equals() {
    assert!(parse_kv("no_equals_here").is_none());
}

#[test]
fn parse_kv_trims_whitespace() {
    let (key, val) = parse_kv("  key  =  \"value\"  ").unwrap();
    assert_eq!(key, "key");
    assert_eq!(val, "value");
}

// ── parse_string_array ────────────────────────────────────

#[test]
fn parse_string_array_simple() {
    let arr = parse_string_array("[\"Draft\", \"Active\", \"Done\"]");
    assert_eq!(arr, vec!["Draft", "Active", "Done"]);
}

#[test]
fn parse_string_array_single_value() {
    let arr = parse_string_array("\"Active\"");
    assert_eq!(arr, vec!["Active"]);
}

#[test]
fn parse_string_array_empty_brackets() {
    let arr = parse_string_array("[]");
    assert!(arr.is_empty());
}

// ── split_inline_tables ───────────────────────────────────

#[test]
fn split_inline_tables_two_items() {
    let result = split_inline_tables("{a = 1}, {b = 2}");
    assert_eq!(result.len(), 2);
    assert_eq!(result[0], "{a = 1}");
    assert_eq!(result[1], "{b = 2}");
}

#[test]
fn split_inline_tables_empty() {
    let result = split_inline_tables("");
    assert!(result.is_empty());
}

// ── parse_inline_fields ───────────────────────────────────

#[test]
fn parse_inline_fields_simple() {
    let map = parse_inline_fields("type = \"schedule\", action = \"Refresh\"");
    assert_eq!(map.get("type").unwrap(), "schedule");
    assert_eq!(map.get("action").unwrap(), "Refresh");
}

#[test]
fn parse_inline_fields_empty() {
    let map = parse_inline_fields("");
    assert!(map.is_empty());
}

// ── join_multiline_arrays ─────────────────────────────────

#[test]
fn join_multiline_single_line() {
    let result = join_multiline_arrays("key = [\"a\", \"b\"]");
    assert_eq!(result.len(), 1);
    assert_eq!(result[0], "key = [\"a\", \"b\"]");
}

#[test]
fn join_multiline_continuation() {
    let input = "effect = [\n  { var = \"x\" },\n]";
    let result = join_multiline_arrays(input);
    assert_eq!(result.len(), 1);
    assert!(result[0].contains("effect = ["));
    assert!(result[0].contains("]"));
}

#[test]
fn join_multiline_no_brackets() {
    let input = "name = \"Test\"\ninitial = \"Draft\"";
    let result = join_multiline_arrays(input);
    assert_eq!(result.len(), 2);
}

// ── parse_guard_clause ────────────────────────────────────

#[test]
fn guard_gt() {
    let g = parse_guard_clause("items > 3").unwrap();
    assert!(matches!(g, Guard::MinCount { ref var, min: 4 } if var == "items"));
}

#[test]
fn guard_gte() {
    let g = parse_guard_clause("items >= 5").unwrap();
    assert!(matches!(g, Guard::MinCount { ref var, min: 5 } if var == "items"));
}

#[test]
fn guard_lt() {
    let g = parse_guard_clause("items < 10").unwrap();
    assert!(matches!(g, Guard::MaxCount { ref var, max: 10 } if var == "items"));
}

#[test]
fn guard_lte() {
    let g = parse_guard_clause("items <= 10").unwrap();
    assert!(matches!(g, Guard::MaxCount { ref var, max: 11 } if var == "items"));
}

#[test]
fn guard_prefix_min() {
    let g = parse_guard_clause("min items 3").unwrap();
    assert!(matches!(g, Guard::MinCount { ref var, min: 3 } if var == "items"));
}

#[test]
fn guard_prefix_max() {
    let g = parse_guard_clause("max items 10").unwrap();
    assert!(matches!(g, Guard::MaxCount { ref var, max: 10 } if var == "items"));
}

#[test]
fn guard_is_true() {
    let g = parse_guard_clause("is_true approved").unwrap();
    assert!(matches!(g, Guard::IsTrue { ref var } if var == "approved"));
}

#[test]
fn guard_list_contains() {
    let g = parse_guard_clause("list_contains tags vip").unwrap();
    assert!(
        matches!(g, Guard::ListContains { ref var, ref value } if var == "tags" && value == "vip")
    );
}

#[test]
fn guard_list_length_min() {
    let g = parse_guard_clause("list_length_min tags 2").unwrap();
    assert!(matches!(g, Guard::ListLengthMin { ref var, min: 2 } if var == "tags"));
}

#[test]
fn guard_bare_boolean() {
    let g = parse_guard_clause("has_mutation").unwrap();
    assert!(matches!(g, Guard::IsTrue { ref var } if var == "has_mutation"));
}

#[test]
fn guard_negation_prefix() {
    let g = parse_guard_clause("!needs_approval").unwrap();
    assert!(matches!(g, Guard::IsFalse { ref var } if var == "needs_approval"));
}

#[test]
fn guard_is_false_prefix() {
    let g = parse_guard_clause("is_false budget_exhausted").unwrap();
    assert!(matches!(g, Guard::IsFalse { ref var } if var == "budget_exhausted"));
}

#[test]
fn guard_unsupported_syntax() {
    assert!(parse_guard_clause("two words bad").is_err());
}
