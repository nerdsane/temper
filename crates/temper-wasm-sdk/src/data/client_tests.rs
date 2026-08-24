use super::*;

#[derive(Debug, PartialEq, serde::Deserialize)]
struct ExampleEntity {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Status")]
    status: String,
    #[serde(rename = "Count")]
    count: i64,
}

#[derive(Debug, PartialEq, serde::Deserialize)]
enum ExampleOutcome {
    Accepted,
}

fn action_result(result: Option<serde_json::Value>, result_omitted: bool) -> DataResultV1 {
    DataResultV1::Action {
        commit: CommitToken {
            entity_type: "Temper.Example.Entity".into(),
            entity_id: "entity-1".into(),
            sequence: 4,
        },
        result,
        result_omitted,
    }
}

#[test]
fn action_decoder_preserves_entity_scalar_enum_void_and_omitted_shapes() {
    let entity = decode_action::<ExampleEntity>(action_result(
        Some(serde_json::json!({"Id":"entity-1","Status":"Ready","Count":2})),
        false,
    ))
    .expect("canonical entity action result decodes");
    assert_eq!(entity.commit.sequence, 4);
    assert_eq!(
        entity.result,
        Some(ExampleEntity {
            id: "entity-1".into(),
            status: "Ready".into(),
            count: 2,
        })
    );

    let scalar = decode_action::<i64>(action_result(Some(serde_json::json!(3)), false))
        .expect("scalar action result decodes");
    assert_eq!(scalar.result, Some(3));
    let outcome =
        decode_action::<ExampleOutcome>(action_result(Some(serde_json::json!("Accepted")), false))
            .expect("enum action result decodes");
    assert_eq!(outcome.result, Some(ExampleOutcome::Accepted));
    let void = decode_action::<serde_json::Value>(action_result(None, false))
        .expect("void action result decodes");
    assert_eq!(void.result, None);
    assert!(!void.result_omitted);
    let omitted = decode_action::<ExampleEntity>(action_result(None, true))
        .expect("deliberately omitted entity result decodes");
    assert_eq!(omitted.result, None);
    assert!(omitted.result_omitted);
    assert_eq!(omitted.commit.sequence, 4);
}

#[test]
fn observed_action_commit_advances_the_next_keyed_read_without_polling() {
    let mut client = DataClient::default();
    client.observe_result(&action_result(None, true));
    let mut read = DataOperationV1::EntityGet {
        entity_type: "Temper.Example.Entity".into(),
        entity_id: "entity-1".into(),
        at_least_sequence: Some(2),
    };

    client.apply_observed_sequence(&mut read);

    assert!(matches!(
        read,
        DataOperationV1::EntityGet {
            at_least_sequence: Some(4),
            ..
        }
    ));
}

#[test]
#[cfg(not(feature = "test-helpers"))]
fn native_client_fails_without_fabricating_authority() {
    let error = DataClient::default()
        .call(DataOperationV1::EntityGet {
            entity_type: "Temper.Task".into(),
            entity_id: "task-1".into(),
            at_least_sequence: None,
        })
        .unwrap_err();
    assert_eq!(error.code, "HostUnavailable");
}
