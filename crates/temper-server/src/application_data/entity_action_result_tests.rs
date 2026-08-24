use std::collections::BTreeSet;

use temper_authz::SecurityContext;
use temper_wasm_sdk::data::{DataOperationKind, DataOperationV1, DataOutcomeV1, DataResultV1};

use super::tests::{call, invocation};

#[tokio::test]
async fn entity_action_result_matches_sequence_aware_keyed_read() {
    let invocation = invocation(
        BTreeSet::from([
            DataOperationKind::EntityCreate,
            DataOperationKind::EntityGet,
            DataOperationKind::ActionInvoke,
        ]),
        SecurityContext::system(),
    );
    let id = "018f1f80-7b2d-7000-8000-000000000001";
    let created = call(
        &invocation,
        DataOperationV1::EntityCreate {
            entity_type: "Temper.Example.Customer".into(),
            value: serde_json::json!({"Id": id, "Name": "Before"})
                .as_object()
                .cloned()
                .expect("fixture create value is an object"),
        },
    )
    .await;
    let DataOutcomeV1::Ok {
        result: DataResultV1::Write {
            commit: created, ..
        },
    } = created.outcome
    else {
        panic!("fixture create should commit")
    };

    let action = call(
        &invocation,
        DataOperationV1::ActionInvoke {
            entity_type: "Temper.Example.Customer".into(),
            entity_id: id.into(),
            action: "Rename".into(),
            expected_sequence: Some(created.sequence),
            params: serde_json::json!({"Name": "After"})
                .as_object()
                .cloned()
                .expect("fixture action parameters are an object"),
        },
    )
    .await;
    let DataOutcomeV1::Ok {
        result:
            DataResultV1::Action {
                commit,
                result: Some(action_value),
                result_omitted: false,
            },
    } = action.outcome
    else {
        panic!("entity-valued action should return its committed value")
    };
    assert_eq!(commit.sequence, created.sequence + 1);
    assert_eq!(action_value["Id"], serde_json::json!(id));
    assert_eq!(action_value["Name"], serde_json::json!("After"));
    assert_eq!(action_value["Status"], serde_json::json!("Active"));
    assert_eq!(action_value["RenameCount"], serde_json::json!(1));
    assert!(action_value.get("id").is_none());
    assert!(action_value.get("status").is_none());

    let read = call(
        &invocation,
        DataOperationV1::EntityGet {
            entity_type: "Temper.Example.Customer".into(),
            entity_id: id.into(),
            at_least_sequence: Some(commit.sequence),
        },
    )
    .await;
    let DataOutcomeV1::Ok {
        result: DataResultV1::Entity { value, sequence },
    } = read.outcome
    else {
        panic!("keyed read should observe the action commit without polling")
    };
    assert_eq!(sequence, commit.sequence);
    assert_eq!(serde_json::Value::Object(value), action_value);
}
