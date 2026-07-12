//! Tests for the atomic initial file-content write path (ARN-247 BLOCKER 4).

use super::*;
use temper_jit::table::TransitionTable;

fn file_state() -> EntityState {
    EntityState {
        entity_type: "File".into(),
        entity_id: "fl-1".into(),
        status: "Created".into(),
        item_count: 0,
        counters: Default::default(),
        booleans: Default::default(),
        lists: Default::default(),
        fields: serde_json::json!({}),
        events: Default::default(),
        total_event_count: 0,
        events_since_snapshot: 0,
        last_snapshot_sequence_nr: 0,
        sequence_nr: 0,
        processed_idempotency_keys: Default::default(),
    }
}

// ARN-247 BLOCKER 4: a tenant whose persisted File spec predates the
// version/lineage params under-declares StreamUpdated. The kernel-synthesized
// params must still land (their keys are kernel-fixed), or every file write
// silently loses version_number/previous_version_id/created_by.
#[test]
fn kernel_synthesized_params_survive_an_under_declared_file_spec() {
    let _guard = temper_runtime::scheduler::install_deterministic_context(247);
    let table = TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "File"
states = ["Created", "Ready"]
initial = "Created"

[[state]]
name = "size_bytes"
type = "counter"
initial = "0"

[[action]]
name = "StreamUpdated"
kind = "input"
from = ["Created", "Ready"]
to = "Ready"
params = ["content_hash", "mime_type"]
"#,
    );
    let mut state = file_state();
    let params = serde_json::json!({
        "content_hash": "abc",
        "mime_type": "text/plain",
        "version_number": 1,
        "previous_version_id": "",
        "created_by": "agent-x",
    });
    let no_xref = std::collections::BTreeMap::new();
    let event = apply_synthetic_file_action(&mut state, &table, "StreamUpdated", params, &no_xref)
        .expect("stream updated");

    // Declared params project normally...
    assert_eq!(state.fields["content_hash"], "abc");
    // ...and the under-declared kernel-synthesized params survive on both the
    // entity fields and the recorded event (so replay reconstructs them).
    assert_eq!(state.fields["version_number"], 1);
    assert_eq!(state.fields["created_by"], "agent-x");
    assert_eq!(event.params["version_number"], 1);
    assert_eq!(event.params["created_by"], "agent-x");
}

// The caller-controlled Create path must NOT force-inject — undeclared create
// params are dropped by the boundary.
#[test]
fn create_params_are_still_filtered() {
    let _guard = temper_runtime::scheduler::install_deterministic_context(247);
    let table = TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "File"
states = ["Created"]
initial = "Created"

[[action]]
name = "Create"
kind = "input"
from = ["Created"]
to = "Created"
params = ["name"]
"#,
    );
    let mut state = file_state();
    let params = serde_json::json!({ "name": "readme", "smuggled": "x" });
    let no_xref = std::collections::BTreeMap::new();
    apply_synthetic_file_action(&mut state, &table, "Create", params, &no_xref).expect("create");
    assert_eq!(state.fields["name"], "readme");
    assert!(state.fields.get("smuggled").is_none());
}
