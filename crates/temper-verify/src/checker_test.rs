use super::*;
use crate::model::build_model_from_ioa;

const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

#[test]
fn test_check_model_completes() {
    let model = build_model_from_ioa(ORDER_IOA, 2).unwrap();
    let result = check_model(&model);
    assert!(result.is_complete, "checker should complete");
    assert!(
        result.states_explored > 0,
        "should explore at least one state"
    );
}

#[test]
fn test_check_model_all_properties_hold() {
    let model = build_model_from_ioa(ORDER_IOA, 2).unwrap();
    let result = check_model(&model);
    assert!(
        result.all_properties_hold,
        "all properties should hold, but got counterexamples: {:?}",
        result.counterexamples,
    );
}

#[test]
fn test_check_model_finds_dead_transitions() {
    let src = r#"
[automaton]
name = "Plan"
states = ["Draft", "Active", "Completed"]
initial = "Draft"

[[state]]
name = "task_count"
type = "counter"
initial = "0"

[[action]]
name = "Activate"
from = ["Draft"]
to = "Active"

[[action]]
name = "Complete"
from = ["Active"]
to = "Completed"
guard = "task_count > 0"
"#;
    let model = build_model_from_ioa(src, 2).unwrap();
    let result = check_model(&model);
    assert!(!result.all_properties_hold);
    assert!(
        result
            .dead_transitions
            .iter()
            .any(|transition| transition.contains("Complete")),
        "expected dead transition for Complete, got {:?}",
        result.dead_transitions
    );
}

#[test]
fn test_cross_entity_guard_does_not_break_local_terminal_proof() {
    let src = r#"
[automaton]
name = "Parent"
states = ["Waiting", "Ready"]
initial = "Waiting"
terminal = ["Waiting"]

[[action]]
name = "ProceedWhenChildDone"
from = ["Waiting"]
to = "Ready"
guard = "empty(child_id) || Child[child_id].status in ['Done']"
"#;
    let model = build_model_from_ioa(src, 2).unwrap();
    let result = check_model(&model);
    assert!(
        result.all_properties_hold,
        "abstract cross-entity guard must not be treated as a locally enabled transition: {result:?}"
    );
    assert!(
        result.dead_transitions.is_empty(),
        "abstract cross-entity transitions should not be reported as dead: {:?}",
        result.dead_transitions
    );
    // The local-terminal proof still holds (`terminal` states use
    // local enablement, where the gate is false), AND the gated edge is now
    // genuinely explored: Ready is reachable, so the model is not silently
    // pruning the state behind the gate.
    assert!(
        states_contains_status(&model, "Ready"),
        "cross-entity gated target state must be reachable in the explored model"
    );
}

const QUOTA: &str = r#"
[automaton]
name = "Workspace"
states = ["Active"]
initial = "Active"

[[state]]
name = "used"
type = "counter"
initial = "0"

[[state]]
name = "quota"
type = "counter"
initial = "0"

[[action]]
name = "SetQuota"
from = ["Active"]
to = "Active"
effect = ["quota = params.quota"]

[[action]]
name = "Use"
from = ["Active"]
to = "Active"
effect = ["used += params.size"]

[[invariant]]
name = "UsageBelowQuota"
assert = "status in ['Active'] => used <= quota"
"#;

#[test]
fn param_driven_effects_are_explored_not_ignored() {
    // An unguarded `used += params.size` can pass the quota: the model
    // must find that, where it used to treat the counter as fixed.
    let model = build_model_from_ioa(QUOTA, 2).unwrap();
    let result = check_model(&model);
    assert!(!result.all_properties_hold, "{result:?}");
    let violation = result
        .counterexamples
        .iter()
        .find(|c| c.property == "Invariants")
        .expect("quota violation");
    let action = violation
        .trace
        .iter()
        .filter_map(|(_, action)| action.as_ref())
        .find(|action| action.name == "Use")
        .expect("the counterexample uses Use");
    assert!(action.params.contains_key("size"), "{action}");
    // The symbolic level agrees.
    let smt = crate::smt::verify_symbolic(QUOTA, 2);
    assert!(
        smt.inductive_invariants
            .iter()
            .any(|(name, holds)| name == "UsageBelowQuota" && !holds),
        "{:?}",
        smt.inductive_invariants
    );
}

#[test]
fn an_invariant_over_a_param_set_counter_verifies_when_guarded() {
    // The quota may be set to any value, but only while nothing is used,
    // and usage grows only below the quota: the invariant holds for every
    // parameter value.
    let guarded = r#"
[automaton]
name = "Workspace"
states = ["Active"]
initial = "Active"

[[state]]
name = "used"
type = "counter"
initial = "0"

[[state]]
name = "quota"
type = "counter"
initial = "0"

[[action]]
name = "SetQuota"
from = ["Active"]
guard = "used == 0"
effect = ["quota = params.quota"]

[[action]]
name = "Use"
from = ["Active"]
guard = "used < quota"
effect = ["used += 1"]

[[invariant]]
name = "UsageBelowQuota"
assert = "used <= quota"
"#;
    let model = build_model_from_ioa(guarded, 2).unwrap();
    let result = check_model(&model);
    assert!(result.all_properties_hold, "{result:?}");
    assert!(result.dead_transitions.is_empty(), "{result:?}");
}

#[test]
fn a_param_appended_list_element_can_match_a_guard_literal() {
    let src = r#"
[automaton]
name = "Ticket"
states = ["Open", "Escalated"]
initial = "Open"

[[state]]
name = "tags"
type = "list"
initial = "[]"

[[action]]
name = "Tag"
from = ["Open"]
effect = ["append(tags, params.tag)"]

[[action]]
name = "Escalate"
from = ["Open"]
to = "Escalated"
guard = "'vip' in tags"
"#;
    let model = build_model_from_ioa(src, 2).unwrap();
    let result = check_model(&model);
    assert!(
        result.dead_transitions.is_empty(),
        "a tag chosen by the caller can be 'vip': {:?}",
        result.dead_transitions
    );
    assert!(states_contains_status(&model, "Escalated"));
}

/// Walk the model's reachable states (mirroring the checker BFS) and report
/// whether `status` is among them.
fn states_contains_status(model: &TemperModel, status: &str) -> bool {
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    for init in model.init_states() {
        if visited.insert(init.clone()) {
            queue.push_back(init);
        }
    }
    while let Some(state) = queue.pop_front() {
        if state.status == status {
            return true;
        }
        for transition in &model.transitions {
            for next in step(model, transition, &state) {
                if visited.insert(next.clone()) {
                    queue.push_back(next);
                }
            }
        }
    }
    false
}

#[test]
fn test_cross_entity_gated_only_target_is_reachable_not_dead() {
    // Published is reachable ONLY through a cross-entity file-ready gate
    // (mirrors a publish transition gated on a related file entity). Before
    // the free-boolean fix the gate lowered to constant-false, so this edge
    // was vacuously dead and Published was never reached. It must now be
    // both reachable and not reported dead.
    let src = r#"
[automaton]
name = "DesignLanguage"
states = ["Draft", "Published"]
initial = "Draft"

[[action]]
name = "Publish"
from = ["Draft"]
to = "Published"
guard = "empty(file_id) || File[file_id].status in ['Ready']"
"#;
    let model = build_model_from_ioa(src, 2).unwrap();
    let result = check_model(&model);

    assert!(
        result.dead_transitions.is_empty(),
        "Publish gated by a cross-entity guard must not be dead: {:?}",
        result.dead_transitions
    );
    assert!(
        result.all_properties_hold,
        "free-boolean cross-entity guard should keep L1 green: {result:?}"
    );
    assert!(
        states_contains_status(&model, "Published"),
        "Published must be reachable through the free-boolean cross-entity edge"
    );
}

#[test]
fn test_liveness_reaches_state_through_cross_entity_gate() {
    // A liveness "eventually reaches Published" property can only be proven
    // if the gated edge is explored. With the free-boolean treatment the
    // ReachesTerminal property holds.
    let src = r#"
[automaton]
name = "DesignLanguage"
states = ["Draft", "Published"]
initial = "Draft"

[[action]]
name = "Publish"
from = ["Draft"]
to = "Published"
guard = "empty(file_id) || File[file_id].status in ['Ready']"

[[liveness]]
name = "EventuallyPublished"
from = ["Draft"]
reaches = ["Published"]
"#;
    let model = build_model_from_ioa(src, 2).unwrap();
    let result = check_model(&model);
    assert!(
        result.all_properties_hold,
        "liveness toward a cross-entity gated state must be provable: {result:?}"
    );
}

#[test]
fn test_cross_entity_transition_dead_when_status_precondition_unreachable() {
    // The free boolean only relaxes the cross-entity conjunct; a transition
    // whose from-state is genuinely never reached is still (correctly) dead.
    let src = r#"
[automaton]
name = "Orphan"
states = ["Start", "Stranded", "End"]
initial = "Start"

[[action]]
name = "Finish"
from = ["Start"]
to = "End"

[[action]]
name = "GatedFromStranded"
from = ["Stranded"]
to = "End"
guard = "empty(other_id) || Other[other_id].status in ['Ok']"
"#;
    let model = build_model_from_ioa(src, 2).unwrap();
    let result = check_model(&model);
    assert!(
        result
            .dead_transitions
            .iter()
            .any(|t| t.contains("GatedFromStranded")),
        "a cross-entity transition out of an unreachable state must still be reported dead: {:?}",
        result.dead_transitions
    );
}
