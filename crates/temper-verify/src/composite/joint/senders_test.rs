use super::*;
use temper_spec::automaton::parse_automaton;

#[test]
fn inherited_source_id_does_not_become_a_different_targets_id() {
    // A[id1] -> B[id1] declares the service. B then forwards the
    // principal to C[id2] through a field, retaining A's id1 at runtime.
    let a = parse_automaton(
        r#"
[automaton]
name = "A"
states = ["Ready", "Done"]
initial = "Ready"
[[action]]
name = "Start"
kind = "input"
from = ["Ready"]
to = "Done"
[[action.triggers]]
name = "first"
kind = "entity"
target_entity = "B"
target_action = "Forward"
principal = "chain"
[action.triggers.resolve_target]
kind = "same_id"
"#,
    )
    .unwrap();
    let policy = SendAnalysis::new(
        r#"
permit(principal == Agent::"service:chain", action, resource) when {
context has triggerSourceEntityId && resource has id &&
context.triggerSourceEntityId == resource.id
};
"#,
    )
    .unwrap();
    let entities = vec!["A".to_string()];
    let senders = Senders::new(&BTreeMap::from([("A", &a)]), &entities, Some(&policy));
    let principal = declared(&a.actions[0].triggers[0], "A", "Start").unwrap();
    let b = senders.reaction(Some(&principal), OUTSIDE, true);
    assert_eq!(senders.may(b, "A", "Start"), May::Yes);
    let c = senders.reaction(None, b, false);
    assert_eq!(senders.may(c, "A", "Start"), May::Maybe);
    // More same-ID hops cannot restore knowledge of the original ID.
    assert_eq!(senders.reaction(None, c, true), c);
    assert_eq!(senders.reaction(None, c, false), c);
    // Same-ID chains preserve knowledge; a declaration starts fresh.
    assert_eq!(senders.reaction(None, b, true), b);
    assert_eq!(senders.reaction(Some(&principal), c, true), b);
}
