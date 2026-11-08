//! A carried assignment may cross a bound even when the model's default
//! parameter choice fits. That is an unmodelled outcome, not a refusal.

use super::super::Sending;

use super::*;
use crate::model::Fields;
use temper_spec::automaton::parse_automaton;

#[test]
fn carried_counts_stay_bounded_without_becoming_definite_refusals() {
    let spec = parse_automaton(
        r#"
[automaton]
name = "Total"
states = ["Ready"]
initial = "Ready"

[[state]]
name = "total"
type = "counter"
initial = 0

[[action]]
name = "Add"
kind = "input"
from = ["Ready"]
to = "Ready"
params = [{ name = "amount", type = "counter" }]
effect = ["total += params.amount"]

# The model holds only a counter something reads.
[[invariant]]
name = "counted"
assert = "total >= 0"
"#,
    )
    .unwrap();
    let model = JointModel::new(
        &BTreeMap::from([("Total", &spec)]),
        &["Total".to_string()].into(),
        "Total",
        &[],
        Sending::anyone(),
        false,
    );
    let mut state = model.inits().next().expect("an initial state");
    let mut dispatch = Dispatch {
        to: "Total".into(),
        action: "Add".into(),
        target_state: None,
        params: Fields::from([("amount", ParamValue::Count(1))]),
        read: true,
        chain: std::sync::Arc::new([]),
        via: None,
        by: 0,
        caller: Some(Fields::new()),
        unread_guard: None,
    };
    for expected in 1..=super::super::JOINT_MAX_COUNTER {
        let outcomes = model.deliver(state, &dispatch, "add");
        assert_eq!(outcomes.len(), 1);
        let (next, _, certainty) = outcomes.into_iter().next().unwrap();
        assert_eq!(certainty, Certainty::Sure);
        assert_eq!(next.records["Total"].counters["total"], expected);
        state = next;
    }
    // Repeated unit sends must stop at the same bound as an outside move.
    let outcomes = model.deliver(state.clone(), &dispatch, "add");
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].0, state);
    assert_eq!(outcomes[0].2, Certainty::Unsure);
    assert!(outcomes[0].1.contains("beyond exploration bounds"));

    // A single large assignment is equally uncertain, even from zero.
    dispatch.params.insert("amount", ParamValue::Count(100));
    let initial = model.inits().next().expect("an initial state");
    let outcomes = model.deliver(initial.clone(), &dispatch, "add");
    assert_eq!(outcomes[0].0, initial);
    assert_eq!(outcomes[0].2, Certainty::Unsure);
}
