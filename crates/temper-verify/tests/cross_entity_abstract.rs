use temper_verify::verify_symbolic;

#[test]
fn cross_entity_guard_marks_symbolic_result_approximate() {
    let spec = r#"
[automaton]
name = "Parent"
states = ["Waiting", "Ready"]
initial = "Waiting"

[[action]]
name = "ProceedWhenChildDone"
from = ["Waiting"]
to = "Ready"
guard = "empty(child_id) || Child[child_id].status in ['Done']"

[[invariant]]
name = "NeverLocallyReady"
assert = "status != 'Ready'"
"#;
    let result = verify_symbolic(spec, 2);

    assert_eq!(
        result
            .guard_satisfiability
            .iter()
            .find(|(name, _)| name == "ProceedWhenChildDone")
            .map(|(_, sat)| *sat),
        Some(true)
    );
    assert!(result.approximate, "{:?}", result.approximation_notes);
    assert!(
        result
            .approximation_notes
            .iter()
            .any(|note| note.contains("cross-entity guards")),
        "expected cross-entity approximation note, got {:?}",
        result.approximation_notes
    );
    assert!(
        result
            .unreachable_states
            .iter()
            .any(|state| state == "Ready"),
        "single-entity reachability must not walk through abstract cross-entity edge"
    );
    assert!(
        result.all_passed,
        "abstract cross-entity edge should not fail a local never-state proof"
    );
}
