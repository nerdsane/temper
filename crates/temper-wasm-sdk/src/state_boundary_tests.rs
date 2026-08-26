use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::json;

use crate::Context;
use crate::data::{DataResultV1, decode_entity};
use crate::schema_deployment::{SchemaMigrationInputV1, SchemaMigrationLogicalContextV1};

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct MemberState {
    status: String,
    task_id: String,
    attempts: usize,
    ready: bool,
    tags: Vec<String>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct SourceState {
    task_id: String,
    revision: usize,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct CsdlEntity {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "TaskId")]
    task_id: String,
}

fn context(entity_state: serde_json::Value) -> Context {
    Context {
        config: BTreeMap::new(),
        trigger_params: json!({}),
        entity_state,
        tenant: "tenant-1".into(),
        entity_type: "Task".into(),
        entity_id: "task-1".into(),
        trigger_action: "Advance".into(),
        wasm_module: "advance_task".into(),
        http_request: None,
    }
}

fn migration_input(canonical_state_json: &str) -> SchemaMigrationInputV1 {
    SchemaMigrationInputV1 {
        abi_version: 1,
        source_bundle_digest: "source".into(),
        target_bundle_digest: "target".into(),
        entity_type: "Temper.Example.Task".into(),
        entity_id: "task-1".into(),
        source_sequence: 7,
        canonical_state_json: canonical_state_json.into(),
        logical_context: SchemaMigrationLogicalContextV1 {
            batch_id: "batch-1".into(),
            item_index: 0,
        },
    }
}

#[test]
fn member_state_flattens_the_exact_runtime_envelope() {
    let ctx = context(json!({
        "entity_type": "Task",
        "entity_id": "task-1",
        "status": "Running",
        "fields": {
            "task_id": "arc-1",
            "attempts": 99,
            "ready": false,
            "tags": ["stale"]
        },
        "counters": {"attempts": 3},
        "booleans": {"ready": true},
        "lists": {"tags": ["verified", "bounded"]},
        "sequence_nr": 7
    }));

    let state = ctx
        .member_state::<MemberState>()
        .expect("canonical member state should decode");

    assert_eq!(
        state,
        MemberState {
            status: "Running".into(),
            task_id: "arc-1".into(),
            attempts: 3,
            ready: true,
            tags: vec!["verified".into(), "bounded".into()],
        }
    );
}

#[test]
fn member_state_rejects_flat_and_pascal_case_legacy_shapes() {
    let flat_error = context(json!({
        "status": "Running",
        "task_id": "arc-1",
        "attempts": 3,
        "ready": true,
        "tags": []
    }))
    .member_state::<MemberState>()
    .expect_err("flat member state must fail");
    assert!(
        flat_error
            .to_string()
            .contains("runtime member-state envelope")
    );

    let pascal_error = context(json!({
        "status": "Running",
        "fields": {"TaskId": "arc-1"},
        "counters": {"Attempts": 3},
        "booleans": {"Ready": true},
        "lists": {"Tags": []}
    }))
    .member_state::<MemberState>()
    .expect_err("PascalCase-only member state must fail");
    assert!(
        pascal_error
            .to_string()
            .contains("invalid typed member state")
    );
}

#[test]
fn migration_source_state_decodes_only_the_canonical_snake_case_object() {
    let state = migration_input(r#"{"task_id":"arc-1","revision":2}"#)
        .source_state::<SourceState>()
        .expect("canonical source state should decode");
    assert_eq!(
        state,
        SourceState {
            task_id: "arc-1".into(),
            revision: 2,
        }
    );

    assert!(
        migration_input(r#"{"TaskId":"arc-1","Revision":2}"#)
            .source_state::<SourceState>()
            .is_err()
    );
}

#[test]
fn odata_entity_decoder_requires_exact_csdl_property_names() {
    let canonical = decode_entity::<CsdlEntity>(DataResultV1::Entity {
        value: json!({"Id": "task-1", "TaskId": "arc-1"})
            .as_object()
            .expect("fixture is an object")
            .clone(),
        sequence: 7,
    })
    .expect("canonical CSDL entity should decode");
    assert_eq!(canonical.value.id, "task-1");

    let error = decode_entity::<CsdlEntity>(DataResultV1::Entity {
        value: json!({"id": "task-1", "task_id": "arc-1"})
            .as_object()
            .expect("fixture is an object")
            .clone(),
        sequence: 7,
    })
    .expect_err("snake_case OData properties must not decode as CSDL properties");
    assert_eq!(error.code, "GeneratedResultTypeMismatch");
}
