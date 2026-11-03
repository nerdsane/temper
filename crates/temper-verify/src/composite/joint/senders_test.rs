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
    let none = BTreeSet::new();
    let senders = Senders::new(
        &BTreeMap::from([("A", &a)]),
        &entities,
        Some(&policy),
        &BTreeSet::new(),
        &none,
        None,
    );
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

#[test]
fn equal_unknown_answers_keep_senders_that_a_later_rule_distinguishes() {
    let note = parse_automaton(
        r#"
[automaton]
name = "Note"
states = ["Unmade", "Posted", "Waiting"]
initial = "Unmade"
[[action]]
name = "Add"
kind = "input"
from = ["Unmade"]
to = "Posted"
[[action]]
name = "Address"
kind = "input"
from = ["Posted"]
to = "Waiting"
"#,
    )
    .unwrap();
    let policy = SendAnalysis::new(
        r#"
permit(principal, action == Action::"Add", resource is Note)
when { principal has role && principal.role in ["member", "visitor"] };
permit(principal, action == Action::"Address", resource is Note)
when { principal has role && principal.role == "member" && context.rule == "Note:Add:address" };
permit(principal, action == Action::"Address", resource is Note)
when { principal has role && principal.role == "visitor" && context.rule == "Note:Add:other" };
"#,
    )
    .unwrap();
    let senders = Senders::new(
        &BTreeMap::from([("Note", &note)]),
        &["Note".to_string()],
        Some(&policy),
        &BTreeSet::from(["Note".to_string()]),
        &BTreeSet::from([("Note".to_string(), "Address".to_string())]),
        None,
    );
    let cause = Cause::reaction(
        false,
        Hop {
            rule: Attr::Value(Value::from("Note:Add:address")),
            ..Hop::unknown()
        },
    );
    let answers: Vec<_> = senders
        .outside()
        .iter()
        .map(|by| {
            policy.may(
                &senders.askers[*by as usize / 2],
                "Address",
                "Note",
                &BTreeMap::new(),
                Some(&cause),
            )
        })
        .collect();
    assert!(
        answers.contains(&May::Yes),
        "member's reaction: {answers:?}"
    );
    assert!(
        answers.contains(&May::No),
        "visitor's reaction: {answers:?}"
    );
}

#[test]
fn sender_answer_tables_reserve_room_before_they_are_allocated() {
    use super::super::room::{Bytes, Construction};

    let mut app = parse_automaton(
        r#"
[automaton]
name = "App"
states = ["Ready"]
initial = "Ready"
[[action]]
name = "Start"
kind = "input"
from = ["Ready"]
to = "Ready"
"#,
    )
    .expect("fixture parses");
    let template = app.actions[0].clone();
    app.actions = (0..512)
        .map(|at| {
            let mut action = template.clone();
            action.name = format!("Action{at}");
            action
        })
        .collect();
    let policies =
        SendAnalysis::new("permit(principal, action, resource);").expect("fixture policies parse");
    let entities = BTreeSet::from(["App".to_string()]);
    // None of these actions is a partition question. They still receive
    // answers in the full tables, even with no outside-kind split.
    let none = BTreeSet::new();
    let room = Bytes::new(4096);
    let memory = Construction::new(&room);
    let reserve = |bytes| memory.set(bytes);
    let senders = Senders::new(
        &BTreeMap::from([("App", &app)]),
        &entities,
        Some(&policies),
        &none,
        &BTreeSet::new(),
        Some(&reserve),
    );
    assert!(
        senders
            .problem
            .as_ref()
            .is_some_and(|p| p.contains("sender answer tables")),
        "table construction must fail before allocation: {:?}",
        senders.problem
    );
    assert!(senders.requests.is_none() && senders.may.is_none());
    drop(senders);
    drop(memory);
    assert_eq!(room.held(), 0, "failed construction releases its room");
}
