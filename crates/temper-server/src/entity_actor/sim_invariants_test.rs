//! Regression seeds for invariant reads across storage representations.
use super::*;
use temper_runtime::scheduler::{DeterministicRng, install_deterministic_context};

const ITEMS: &str = r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted"]
initial = "Draft"
[[state]]
name = "items"
type = "counter"
initial = 1
[[action]]
name = "Submit"
from = ["Draft"]
to = "Submitted"
[[action]]
name = "Drain"
from = ["Draft"]
effect = ["items -= 1"]
[[invariant]]
name = "HasItems"
assert = "items > 0"
[[invariant]]
name = "SubmittedHasItems"
assert = "status == 'Submitted' => items > 0"
"#;

const STRINGS: &str = r#"
[automaton]
name = "Document"
states = ["Draft", "Approved"]
initial = "Draft"
[[state]]
name = "code"
type = "string"
initial = ""
overflow_inline_max_bytes = 8
[[state]]
name = "approved_code"
type = "string"
initial = ""
overflow_inline_max_bytes = 1024
[[action]]
name = "Write"
from = ["Draft"]
params = ["code", "approved_code"]
[[action]]
name = "Approve"
from = ["Draft"]
to = "Approved"
params = ["code", "approved_code"]
[[action]]
name = "Edit"
from = ["Approved"]
params = ["code", "approved_code"]
[[invariant]]
name = "CodeSeen"
assert = "status == 'Approved' => code == approved_code"
"#;

fn handler(spec: &str) -> EntityActorHandler {
    EntityActorHandler::new(
        "Document",
        "old",
        Arc::new(TransitionTable::from_ioa_source(spec)),
    )
    .with_field_sync_mode(FieldSyncMode::blob_refs_default())
}

#[test]
fn invariant_declared_items_allows_submission_of_an_older_record() {
    let _guard = install_deterministic_context(0);
    let mut actor = handler(ITEMS);
    // A record created before items was declared.
    actor.state.counters.clear();
    actor.state.fields.as_object_mut().unwrap().remove("items");
    actor.state.item_count = 0;
    actor.handle_message("Submit", "{}").unwrap();
    assert_eq!(actor.current_status(), "Submitted");
}

#[test]
fn invariant_declared_items_refuses_draining_an_older_record() {
    let _guard = install_deterministic_context(0);
    let mut actor = handler(ITEMS);
    actor.state.counters.clear();
    actor.state.fields.as_object_mut().unwrap().remove("items");
    actor.state.item_count = 0;
    let before = serde_json::to_value(&actor.state).unwrap();
    let error = actor.handle_message("Drain", "{}").unwrap_err();
    assert!(error.contains("HasItems"), "{error}");
    assert_eq!(serde_json::to_value(&actor.state).unwrap(), before);
    // Explicit stored zero remains a pre-existing violation.
    actor.state.counters.insert("items".into(), 0);
    actor.handle_message("Drain", "{}").unwrap();
}

#[test]
fn invariant_blob_values_match_inline_values_on_the_pending_move() {
    let _guard = install_deterministic_context(0);
    let mut actor = handler(STRINGS);
    let value = "identical logical value";
    actor
        .handle_message(
            "Approve",
            &serde_json::json!({
                "code": value, "approved_code": value
            })
            .to_string(),
        )
        .unwrap();
    assert!(crate::blobs::field_overflow_descriptor(&actor.state.fields["code"]).is_some());
    assert_eq!(actor.state.fields["approved_code"], value);
}

#[test]
fn invariant_blob_values_match_inline_values_on_later_moves() {
    let _guard = install_deterministic_context(0);
    let mut actor = handler(STRINGS);
    let value = "identical logical value";
    actor
        .handle_message(
            "Write",
            &serde_json::json!({
                "code": value, "approved_code": value
            })
            .to_string(),
        )
        .unwrap();
    actor.handle_message("Approve", "{}").unwrap();
    let before = serde_json::to_value(&actor.state).unwrap();
    let blobs = actor.overflow_blobs.len();
    let error = actor
        .handle_message("Edit", r#"{"code":"different logical value"}"#)
        .unwrap_err();
    assert!(error.contains("CodeSeen"), "{error}");
    assert_eq!(serde_json::to_value(&actor.state).unwrap(), before);
    assert_eq!(actor.overflow_blobs.len(), blobs);
}

#[test]
fn invariant_blob_values_are_checked_across_seeded_edits_and_read_faults() {
    for seed in 0..32 {
        let _guard = install_deterministic_context(seed);
        let mut rng = DeterministicRng::new(seed);
        let mut actor = handler(STRINGS);
        let initial = "initial logical value";
        actor
            .handle_message(
                "Approve",
                &serde_json::json!({
                    "code": initial, "approved_code": initial
                })
                .to_string(),
            )
            .unwrap();
        for step in 0..16 {
            let choice = rng.next_u64() % 4;
            let value = format!("logical value {seed} {step}");
            let saved = actor.overflow_blobs.clone();
            if choice == 2 {
                actor.overflow_blobs.clear();
            } else if choice == 3 {
                actor.overflow_blobs[0].body[0] ^= 1;
            }
            let params = if choice == 0 {
                serde_json::json!({"code": value, "approved_code": value})
            } else {
                serde_json::json!({"approved_code": value})
            };
            let before = serde_json::to_value(&actor.state).unwrap();
            let result = actor.handle_message("Edit", &params.to_string());
            assert_eq!(
                result.is_ok(),
                choice == 0,
                "seed={seed} step={step}: {result:?}"
            );
            if choice != 0 {
                assert_eq!(serde_json::to_value(&actor.state).unwrap(), before);
                actor.overflow_blobs = saved;
            }
        }
    }
}

#[test]
fn invariant_default_references_resolve_for_older_simulated_records() {
    let _guard = install_deterministic_context(0);
    for (declaration, stored) in [
        (
            r#"type = "string"
initial = "p1""#,
            None,
        ),
        (
            r#"type = "list"
initial = ["p1"]"#,
            None,
        ),
        (
            r#"type = "string"
initial = "closed""#,
            Some(serde_json::json!("p1")),
        ),
    ] {
        let spec = format!(
            r#"
[automaton]
name = "Child"
states = ["Draft", "Submitted"]
initial = "Draft"
[[state]]
name = "parent_id"
{declaration}
[[action]]
name = "Submit"
from = ["Draft"]
to = "Submitted"
[[invariant]]
name = "OpenParent"
assert = "status == 'Submitted' => Parent[parent_id].status == 'Open'"
"#
        );
        let mut actor = handler(&spec);
        actor
            .state
            .fields
            .as_object_mut()
            .unwrap()
            .remove("parent_id");
        actor.state.lists.remove("parent_id");
        if let Some(stored) = stored {
            actor.state.fields["parent_id"] = stored;
        }
        let before = serde_json::to_value(&actor.state).unwrap();
        actor.refresh_related_statuses(&BTreeMap::from([(
            ("Parent".into(), "p1".into()),
            "Closed".into(),
        )]));
        assert!(
            actor
                .handle_message("Submit", "{}")
                .unwrap_err()
                .contains("OpenParent")
        );
        assert_eq!(serde_json::to_value(&actor.state).unwrap(), before);
        actor.refresh_related_statuses(&BTreeMap::from([(
            ("Parent".into(), "p1".into()),
            "Open".into(),
        )]));
        actor.handle_message("Submit", "{}").unwrap();
    }
}

#[tokio::test]
async fn invariant_blob_values_resolve_from_the_committed_store() {
    use crate::blobs::BlobReadSource;
    use crate::entity_actor::action_input::process_action_with_blob_prestate;
    let _guard = install_deterministic_context(0);
    let dir = tempfile::tempdir().unwrap();
    let store = crate::blob_store::BlobStore::local_fs(dir.path());
    let mut actor = handler(STRINGS);
    let mode = FieldSyncMode::blob_refs_default();
    let value = "committed logical value";
    let written = process_action_with_blob_prestate(
        &mut actor.state,
        &actor.table,
        "Write",
        &serde_json::json!({"code": value, "approved_code": value}),
        &RelatedMap::new(),
        mode,
        BlobReadSource::Store(&store),
    )
    .await;
    assert!(written.success, "{:?}", written.error);
    crate::blobs::put_overflow_blobs(&store, &written.overflow_blobs)
        .await
        .unwrap();
    let approved = process_action_with_blob_prestate(
        &mut actor.state,
        &actor.table,
        "Approve",
        &serde_json::json!({}),
        &RelatedMap::new(),
        mode,
        BlobReadSource::Store(&store),
    )
    .await;
    assert!(approved.success, "{:?}", approved.error);
    let before = serde_json::to_value(&actor.state).unwrap();
    let refused = process_action_with_blob_prestate(
        &mut actor.state,
        &actor.table,
        "Edit",
        &serde_json::json!({"approved_code": "a different value"}),
        &RelatedMap::new(),
        mode,
        BlobReadSource::Store(&store),
    )
    .await;
    assert!(!refused.success);
    assert!(matches!(refused.refusal,
        Some(temper_jit::table::Refusal::InvariantViolated { ref invariant, .. }) if invariant == "CodeSeen"));
    assert_eq!(serde_json::to_value(&actor.state).unwrap(), before);
    assert!(refused.overflow_blobs.is_empty());
}

#[test]
fn invariant_already_broken_logical_blob_values_still_allow_moves() {
    let _guard = install_deterministic_context(0);
    let mut actor = handler(STRINGS);
    actor
        .handle_message(
            "Write",
            r#"{"code":"a long logical value","approved_code":"different"}"#,
        )
        .unwrap();
    // A persisted record from before CodeSeen existed.
    actor.state.status = "Approved".into();
    actor.handle_message("Edit", "{}").unwrap();
    assert_eq!(actor.current_status(), "Approved");
}
