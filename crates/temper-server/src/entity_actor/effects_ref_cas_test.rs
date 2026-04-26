use super::*;

fn ref_table() -> TransitionTable {
    let spec = r#"
[automaton]
name = "Ref"
states = ["Active", "Deleted"]
initial = "Active"

[[action]]
name = "Update"
from = ["Active"]
to = "Active"
params = ["PreviousCommitSha", "NewCommitSha"]

[[action]]
name = "ForceUpdate"
from = ["Active"]
to = "Active"
params = ["NewCommitSha"]

[[action]]
name = "Delete"
from = ["Active"]
to = "Deleted"
params = ["PreviousCommitSha"]
"#;
    temper_jit::table::TransitionTable::from_ioa_source(spec)
}

fn ref_state(target_sha: &str) -> EntityState {
    EntityState {
        entity_type: "Ref".into(),
        entity_id: "ref-main".into(),
        status: "Active".into(),
        item_count: 0,
        counters: std::collections::BTreeMap::new(),
        booleans: std::collections::BTreeMap::new(),
        lists: std::collections::BTreeMap::new(),
        fields: serde_json::json!({
            "RepositoryId": "rp-owner-repo",
            "Name": "refs/heads/main",
            "TargetCommitSha": target_sha,
            "Status": "Active",
        }),
        events: std::collections::VecDeque::new(),
        total_event_count: 0,
        sequence_nr: 0,
    }
}

#[test]
fn ref_update_requires_matching_previous_sha_and_projects_target() {
    let table = ref_table();
    let old_sha = "1111111111111111111111111111111111111111";
    let new_sha = "2222222222222222222222222222222222222222";
    let mut state = ref_state(old_sha);

    let result = process_action(
        &mut state,
        &table,
        "Update",
        &serde_json::json!({
            "PreviousCommitSha": old_sha,
            "NewCommitSha": new_sha,
        }),
    );

    assert!(result.success, "matching CAS update should succeed");
    assert_eq!(
        state.fields["TargetCommitSha"].as_str(),
        Some(new_sha),
        "Update should advance the Git-visible target field"
    );
}

#[test]
fn ref_update_rejects_stale_previous_sha() {
    let table = ref_table();
    let current_sha = "1111111111111111111111111111111111111111";
    let stale_sha = "0000000000000000000000000000000000000000";
    let new_sha = "2222222222222222222222222222222222222222";
    let mut state = ref_state(current_sha);

    let result = process_action(
        &mut state,
        &table,
        "Update",
        &serde_json::json!({
            "PreviousCommitSha": stale_sha,
            "NewCommitSha": new_sha,
        }),
    );

    assert!(!result.success, "stale CAS update should fail");
    assert_eq!(state.fields["TargetCommitSha"].as_str(), Some(current_sha));
    assert_eq!(
        result.error.as_deref(),
        Some(
            "stale ref compare-and-swap: expected 0000000000000000000000000000000000000000, current 1111111111111111111111111111111111111111"
        )
    );
}

#[test]
fn ref_delete_requires_matching_previous_sha() {
    let table = ref_table();
    let current_sha = "1111111111111111111111111111111111111111";
    let stale_sha = "0000000000000000000000000000000000000000";
    let mut state = ref_state(current_sha);

    let result = process_action(
        &mut state,
        &table,
        "Delete",
        &serde_json::json!({
            "PreviousCommitSha": stale_sha,
        }),
    );

    assert!(!result.success, "stale CAS delete should fail");
    assert_eq!(state.status, "Active");
    assert_eq!(state.fields["TargetCommitSha"].as_str(), Some(current_sha));
}

#[test]
fn ref_force_update_projects_target_without_cas() {
    let table = ref_table();
    let old_sha = "1111111111111111111111111111111111111111";
    let new_sha = "2222222222222222222222222222222222222222";
    let mut state = ref_state(old_sha);

    let result = process_action(
        &mut state,
        &table,
        "ForceUpdate",
        &serde_json::json!({
            "NewCommitSha": new_sha,
        }),
    );

    assert!(result.success, "force update should bypass CAS");
    assert_eq!(state.fields["TargetCommitSha"].as_str(), Some(new_sha));
}
