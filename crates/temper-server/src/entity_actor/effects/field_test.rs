use super::super::types::EntityState;
use super::*;
use temper_jit::table::TransitionTable;

fn make_state(entity_type: &str, entity_id: &str) -> EntityState {
    EntityState {
        entity_type: entity_type.into(),
        entity_id: entity_id.into(),
        status: "Initial".into(),
        item_count: 0,
        counters: std::collections::BTreeMap::new(),
        booleans: std::collections::BTreeMap::new(),
        lists: std::collections::BTreeMap::new(),
        fields: serde_json::json!({}),
        events: std::collections::VecDeque::new(),
        total_event_count: 0,
        events_since_snapshot: 0,
        last_snapshot_sequence_nr: 0,
        sequence_nr: 0,
        processed_idempotency_keys: std::collections::BTreeMap::new(),
    }
}

fn ref_transition_table() -> TransitionTable {
    let spec = r#"
[automaton]
name = "Ref"
states = ["Active", "Deleted"]
initial = "Active"

[[action]]
name = "Update"
kind = "input"
from = ["Active"]
to = "Active"
params = ["PreviousCommitSha", "NewCommitSha"]

[[action]]
name = "ForceUpdate"
kind = "input"
from = ["Active"]
to = "Active"
params = ["NewCommitSha"]
"#;

    TransitionTable::from_ioa_source(spec)
}

fn active_ref_state(target: &str) -> EntityState {
    let mut state = make_state("Ref", "rf-repo-refs-heads-main");
    state.status = "Active".to_string();
    state.fields = serde_json::json!({ "TargetCommitSha": target });
    state
}

#[test]
fn ref_update_advances_target_commit_sha_after_matching_cas() {
    let table = ref_transition_table();
    let current = "1111111111111111111111111111111111111111";
    let next = "2222222222222222222222222222222222222222";
    let mut state = active_ref_state(current);

    let result = process_action(
        &mut state,
        &table,
        "Update",
        &serde_json::json!({
            "PreviousCommitSha": current,
            "NewCommitSha": next
        }),
    );

    assert!(result.success, "matching CAS update should succeed");
    assert_eq!(
        state.fields.get("TargetCommitSha").and_then(|v| v.as_str()),
        Some(next)
    );
    assert_eq!(
        result
            .event
            .as_ref()
            .and_then(|event| event.params.get("TargetCommitSha"))
            .and_then(|value| value.as_str()),
        Some(next)
    );
}

#[test]
fn ref_update_rejects_stale_previous_commit_sha() {
    let table = ref_transition_table();
    let current = "1111111111111111111111111111111111111111";
    let stale = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let next = "2222222222222222222222222222222222222222";
    let mut state = active_ref_state(current);

    let result = process_action(
        &mut state,
        &table,
        "Update",
        &serde_json::json!({
            "PreviousCommitSha": stale,
            "NewCommitSha": next
        }),
    );

    assert!(!result.success, "stale CAS update should fail");
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("stale ref")),
        "unexpected error: {:?}",
        result.error
    );
    assert_eq!(
        state.fields.get("TargetCommitSha").and_then(|v| v.as_str()),
        Some(current)
    );
}

#[test]
fn ref_force_update_advances_target_commit_sha() {
    let table = ref_transition_table();
    let current = "1111111111111111111111111111111111111111";
    let next = "2222222222222222222222222222222222222222";
    let mut state = active_ref_state(current);

    let result = process_action(
        &mut state,
        &table,
        "ForceUpdate",
        &serde_json::json!({
            "NewCommitSha": next
        }),
    );

    assert!(result.success, "force update should succeed");
    assert_eq!(
        state.fields.get("TargetCommitSha").and_then(|v| v.as_str()),
        Some(next)
    );
}

#[test]
fn default_field_inline_max_is_128kb() {
    assert_eq!(DEFAULT_FIELD_INLINE_MAX, 131_072);
}

#[test]
fn blob_refs_default_carries_default_ceiling() {
    let mode = FieldSyncMode::blob_refs_default();
    assert_eq!(
        mode,
        FieldSyncMode::BlobRefs {
            default_inline_max: DEFAULT_FIELD_INLINE_MAX
        }
    );
}

#[test]
fn field_under_ceiling_stays_inline_blob_refs() {
    let mut state = make_state("Session", "s-1");
    let under = "x".repeat(64 * 1024); // 64 KB, under 128 KB ceiling
    let params = serde_json::json!({ "user_message": under });

    let overflow = sync_fields(&mut state, &params, FieldSyncMode::blob_refs_default());

    assert!(
        overflow.is_empty(),
        "no blob overflow for field under ceiling"
    );
    assert_eq!(
        state
            .fields
            .get("user_message")
            .and_then(|v| v.as_str())
            .map(str::len),
        Some(64 * 1024),
        "inline value preserved"
    );
}

#[test]
fn field_over_default_ceiling_overflows_to_blob() {
    let mut state = make_state("Session", "s-1");
    let over = "y".repeat(200 * 1024); // 200 KB, over 128 KB ceiling
    let params = serde_json::json!({ "user_message": over });

    let overflow = sync_fields(&mut state, &params, FieldSyncMode::blob_refs_default());

    assert_eq!(overflow.len(), 1, "one overflow blob written");
    let ref_obj = state
        .fields
        .get("user_message")
        .and_then(|v| v.as_object())
        .expect("blob ref object present");
    assert!(ref_obj.contains_key(crate::blobs::FIELD_OVERFLOW_REF_KEY));
    assert!(ref_obj.contains_key(crate::blobs::FIELD_OVERFLOW_SIZE_KEY));
}

#[test]
fn field_over_legacy_32k_stays_inline_under_new_ceiling() {
    // Regression test for ADR-0045: fields in the 32KB-128KB band that
    // previously overflowed now stay inline.
    let mut state = make_state("Session", "s-1");
    let mid = "z".repeat(80 * 1024); // 80 KB — above old 32KB cap, below new 128KB
    let params = serde_json::json!({ "mid_field": mid });

    let overflow = sync_fields(&mut state, &params, FieldSyncMode::blob_refs_default());

    assert!(
        overflow.is_empty(),
        "80KB field stays inline under new ceiling"
    );
    assert_eq!(
        state
            .fields
            .get("mid_field")
            .and_then(|v| v.as_str())
            .map(str::len),
        Some(80 * 1024)
    );
}

#[test]
fn inline_truncate_mode_truncates_and_warns_above_ceiling() {
    let mut state = make_state("Session", "s-1");
    let huge = "q".repeat(200 * 1024);
    let params = serde_json::json!({ "user_message": huge });

    let overflow = sync_fields(&mut state, &params, FieldSyncMode::InlineTruncate);

    assert!(overflow.is_empty(), "InlineTruncate never writes blobs");
    let v = state
        .fields
        .get("user_message")
        .and_then(|v| v.as_str())
        .expect("truncation produces a string placeholder");
    assert!(v.starts_with("[truncated:"), "placeholder shape preserved");
}

#[test]
fn repository_receive_pack_fields_are_transient() {
    let mut state = make_state("Repository", "rp-acme-app");
    state.fields = serde_json::json!({
        "OwnerAccountId": "acme",
        "Name": "app",
        "DefaultBranch": "main",
        "PackBytes": "stale-pack",
        "RefUpdates": [{"Name": "refs/heads/main"}],
        "ClientRequestId": "stale-request"
    });
    let params = serde_json::json!({
        "PackBytes": "fresh-pack",
        "RefUpdates": [{"Name": "refs/heads/main", "NewCommitSha": "abc"}],
        "ClientRequestId": "fresh-request"
    });

    let overflow = sync_fields(&mut state, &params, FieldSyncMode::blob_refs_default());

    assert!(overflow.is_empty());
    assert!(state.fields.get("PackBytes").is_none());
    assert!(state.fields.get("RefUpdates").is_none());
    assert!(state.fields.get("ClientRequestId").is_none());
    assert_eq!(
        state.fields.get("OwnerAccountId").and_then(|v| v.as_str()),
        Some("acme")
    );
}

#[test]
fn custom_inline_max_overrides_default() {
    // A caller constructing BlobRefs with a non-default ceiling must see
    // that ceiling applied, not the crate default.
    let mut state = make_state("Session", "s-1");
    let mid = "m".repeat(50 * 1024); // 50 KB
    let params = serde_json::json!({ "mid_field": mid });

    let tight = FieldSyncMode::BlobRefs {
        default_inline_max: 32 * 1024, // 32 KB — tighter than default
    };
    let overflow = sync_fields(&mut state, &params, tight);

    assert_eq!(overflow.len(), 1, "50KB overflows under 32KB ceiling");
}

#[test]
fn oversize_list_field_also_overflows_to_blob() {
    // Regression guard: sync_fields threads project_field_value through the
    // lists loop as well as the params loop. Both branches must respect
    // the ceiling.
    let mut state = make_state("Session", "s-1");
    let big = "L".repeat(10 * 1024);
    state.lists.insert(
        "tool_outputs".to_string(),
        (0..16).map(|_| big.clone()).collect(), // 160 KB serialized
    );

    let overflow = sync_fields(
        &mut state,
        &serde_json::json!({}),
        FieldSyncMode::blob_refs_default(),
    );

    assert_eq!(overflow.len(), 1, "oversize list overflows to blob");
    let ref_obj = state
        .fields
        .get("tool_outputs")
        .and_then(|v| v.as_object())
        .expect("blob ref object present for list field");
    assert!(ref_obj.contains_key(crate::blobs::FIELD_OVERFLOW_REF_KEY));
}

#[test]
fn duplicate_oversize_value_produces_single_blob_write() {
    // Content-addressed dedupe: two params with identical oversize content
    // share one blob write.
    let mut state = make_state("Session", "s-1");
    let big = "d".repeat(200 * 1024);
    let params = serde_json::json!({ "a": &big, "b": &big });

    let overflow = sync_fields(&mut state, &params, FieldSyncMode::blob_refs_default());

    assert_eq!(overflow.len(), 1, "dedupe by content hash");
}
