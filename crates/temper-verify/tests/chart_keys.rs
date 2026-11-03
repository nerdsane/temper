//! `says` and `[[group]]` only label a spec for drawing: the cascade
//! verifies a spec that has them exactly as it verifies one without.

use temper_verify::VerificationCascade;

const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

/// The Order fixture with a labelled action and nested groups.
fn order_with_chart_keys() -> String {
    let mut spec = ORDER_IOA.replacen(
        "name = \"SubmitOrder\"\n",
        "name = \"SubmitOrder\"\nsays = \"send it in\"\n",
        1,
    );
    spec.push_str(
        r#"
[[group]]
name = "Live"
says = "while the order is live"
states = ["Open", "Processing"]

[[group]]
name = "Open"
states = ["Draft", "Submitted", "Confirmed"]
"#,
    );
    spec
}

fn level_summaries(spec: &str) -> Vec<(bool, String)> {
    VerificationCascade::from_ioa(spec)
        .with_sim_seeds(3)
        .with_prop_test_cases(50)
        .run()
        .levels
        .into_iter()
        .map(|level| (level.passed, level.summary))
        .collect()
}

#[test]
fn the_cascade_verifies_a_spec_with_says_and_groups_like_one_without() {
    let labelled = level_summaries(&order_with_chart_keys());
    let plain = level_summaries(ORDER_IOA);
    assert_eq!(labelled.len(), 4, "L0 to L3 all run: {labelled:#?}");
    for (passed, summary) in &labelled {
        assert!(passed, "level failed: {summary}");
    }
    assert_eq!(labelled, plain);
}
