use serde_json::json;

use super::runner::collapse_runtime_alias;

#[test]
fn migration_boundary_retains_only_the_snake_case_runtime_name() {
    let mut fields = json!({
        "Id": "task-1",
        "id": "task-1",
        "Status": "Ready",
        "status": "Ready"
    })
    .as_object()
    .expect("fixture is an object")
    .clone();

    collapse_runtime_alias(&mut fields, "Id", "id").expect("matching identity aliases");
    collapse_runtime_alias(&mut fields, "Status", "status").expect("matching lifecycle aliases");

    assert_eq!(fields.get("id"), Some(&json!("task-1")));
    assert_eq!(fields.get("status"), Some(&json!("Ready")));
    assert!(!fields.contains_key("Id"));
    assert!(!fields.contains_key("Status"));
}

#[test]
fn migration_boundary_renames_a_pascal_only_runtime_field() {
    let mut fields = json!({"Id": "task-1"})
        .as_object()
        .expect("fixture is an object")
        .clone();

    collapse_runtime_alias(&mut fields, "Id", "id").expect("legacy identity is canonicalized");

    assert_eq!(fields.get("id"), Some(&json!("task-1")));
    assert!(!fields.contains_key("Id"));
}

#[test]
fn migration_boundary_rejects_disagreeing_runtime_aliases() {
    let mut fields = json!({"Id": "task-1", "id": "task-2"})
        .as_object()
        .expect("fixture is an object")
        .clone();

    let error = collapse_runtime_alias(&mut fields, "Id", "id")
        .expect_err("disagreeing identity aliases must fail");

    assert_eq!(error.code(), "migration_rejected");
}
