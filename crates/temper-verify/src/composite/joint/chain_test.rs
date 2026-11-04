use super::*;
use temper_spec::automaton::parse_automaton;

fn chain_specs() -> Vec<temper_spec::automaton::Automaton> {
    let a = r#"
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
"#;
    let b = r#"
[automaton]
name = "B"
states = ["Ready", "Done"]
initial = "Ready"
[[state]]
name = "destination"
type = "string"
initial = ""
[[action]]
name = "Forward"
kind = "input"
from = ["Ready"]
to = "Done"
[[action.triggers]]
name = "second"
kind = "entity"
target_entity = "C"
target_action = "Finish"
[action.triggers.resolve_target]
kind = "field"
id_field = "destination"
"#;
    let c = r#"
[automaton]
name = "C"
states = ["Ready", "Done"]
initial = "Ready"
[[action]]
name = "Finish"
kind = "input"
from = ["Ready"]
to = "Done"
"#;
    [a, b, c]
        .iter()
        .map(|text| parse_automaton(text).unwrap())
        .collect()
}

#[test]
fn a_chain_to_a_distinct_id_can_be_refused_by_cedar() {
    let automatons = chain_specs();
    let by_name = automatons
        .iter()
        .map(|a| (a.automaton.name.as_str(), a))
        .collect();
    let policies = SendAnalysis::new(
        r#"
permit(principal, action == Action::"Start", resource);
permit(principal == Agent::"service:chain", action, resource) when {
    context has triggerSourceEntityId && resource has id &&
    context.triggerSourceEntityId == resource.id
};
"#,
    )
    .unwrap();
    let model = JointModel::new(
        &by_name,
        &BTreeSet::from(["A".into(), "B".into(), "C".into()]),
        "A",
        &[],
        Sending {
            policies: Some(&policies),
            reserve: None,
            kernel_moves: true,
            relies_on: BTreeSet::new(),
        },
        false,
    );
    for same_id in [false, true] {
        let ids = BTreeMap::from([
            ("A".into(), "id1".into()),
            ("B".into(), "id1".into()),
            ("C".into(), if same_id { "id1" } else { "id2" }.into()),
        ]);
        let (mut state, _) = model.init_with_ids(&ids);
        let destination = state.records["C"].strings["Id"].clone();
        std::rc::Rc::make_mut(state.records.get_mut("B").unwrap())
            .strings
            .insert("destination".into(), destination);
        // Start and Forward each have one permitted outcome.
        for _ in 0..2 {
            let moves = model.moves(&state);
            assert_eq!(moves.len(), 1);
            let (mut next, _) = model.next(&state, &moves[0]);
            assert_eq!(next.len(), 1);
            state = next.remove(0).state;
        }
        assert_eq!(state.records["B"].status, "Done");
        assert_eq!(state.inflight.len(), 1);
        let moves = model.moves(&state);
        assert_eq!(moves.len(), 1);
        let (next, _) = model.next(&state, &moves[0]);
        // A field hop forgets that the source id is the receiver's, so the
        // precomputed answer is Maybe. It is decided when it lands, from A's
        // id (the one the inherited service keeps) and C's: Cedar refuses
        // id1 != id2 and allows id1 == id1.
        let refused = next
            .iter()
            .any(|n| n.state.records["C"].status == "Ready" && n.state.inflight.is_empty());
        let done = next.iter().any(|n| n.state.records["C"].status == "Done");
        assert_eq!((refused, done), (!same_id, same_id), "same_id {same_id}");
    }
}

#[test]
fn an_inherited_service_keeps_its_source_apart_from_the_moves_cause() {
    let mut automatons = chain_specs();
    // A[a] declares the service while finding B[b] through a field.
    // B forwards to C[c] without declaring another principal.
    // Declare the tracked field through the same parser as production specs.
    let a = r#"
[automaton]
name = "A"
states = ["Ready", "Done"]
initial = "Ready"
[[state]]
name = "destination"
type = "string"
initial = ""
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
kind = "field"
id_field = "destination"
"#;
    automatons[0] = parse_automaton(a).unwrap();
    for same_id in [false, true] {
        automatons[1].actions[0].triggers[0].resolve_target = Some(if same_id {
            temper_spec::automaton::TargetResolver::SameId
        } else {
            temper_spec::automaton::TargetResolver::Field {
                id_field: "destination".into(),
            }
        });
        let by_name = automatons
            .iter()
            .map(|a| (a.automaton.name.as_str(), a))
            .collect();
        // Only A's id changes between these questions. B, C and the rule stay
        // the same, so a cached answer must include the inherited source too.
        for permitted_source in ["b", "a"] {
            let policies = SendAnalysis::new(&format!(
                r#"
permit(principal, action == Action::"Start", resource);
permit(principal == Agent::"service:chain", action == Action::"Forward", resource);
permit(principal == Agent::"service:chain", action == Action::"Finish", resource) when {{
    context.triggerSourceEntityType == "A" &&
    context.triggerSourceEntityId == "{permitted_source}" &&
    context.causeEntityType == "B" && context.causeEntityId == "b" &&
    context.rule == "B:Forward:second"
}};
"#,
            ))
            .unwrap();
            let model = JointModel::new(
                &by_name,
                &BTreeSet::from(["A".into(), "B".into(), "C".into()]),
                "A",
                &[],
                Sending {
                    policies: Some(&policies),
                    reserve: None,
                    kernel_moves: true,
                    relies_on: BTreeSet::new(),
                },
                false,
            );
            for source in ["a", "b", "a"] {
                let ids = BTreeMap::from([
                    ("A".into(), source.into()),
                    ("B".into(), "b".into()),
                    ("C".into(), if same_id { "b" } else { "c" }.into()),
                ]);
                let (mut state, _) = model.init_with_ids(&ids);
                for (entity, id) in &ids {
                    std::rc::Rc::make_mut(state.records.get_mut(entity.as_str()).unwrap())
                        .strings
                        .insert("Id".into(), crate::model::ModelString::from(id.clone()));
                }
                for (from, to) in [("A", "B"), ("B", "C")] {
                    let destination = state.records[to].strings["Id"].clone();
                    std::rc::Rc::make_mut(state.records.get_mut(from).unwrap())
                        .strings
                        .insert("destination".into(), destination);
                }
                for _ in 0..2 {
                    let moves = model.moves(&state);
                    assert_eq!(moves.len(), 1);
                    let (mut next, _) = model.next(&state, &moves[0]);
                    assert_eq!(next.len(), 1);
                    state = next.remove(0).state;
                }
                let moves = model.moves(&state);
                assert_eq!(moves.len(), 1);
                let (next, _) = model.next(&state, &moves[0]);
                assert_eq!(next.len(), 1);
                let expected = if source == permitted_source {
                    "Done"
                } else {
                    "Ready"
                };
                assert_eq!(
                    next[0].state.records["C"].status, expected,
                    "inherited source {source}, permitted source {permitted_source}"
                );
            }
        }
    }
}
