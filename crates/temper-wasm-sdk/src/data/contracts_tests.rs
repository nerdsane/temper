use super::*;
use crate::data::DataResponseV1;

#[test]
fn exact_entity_get_encoding() {
    let request = DataRequestV1::new(DataOperationV1::EntityGet {
        entity_type: "Temper.App.Task".into(),
        entity_id: "task-1".into(),
        at_least_sequence: Some(7),
    });
    assert_eq!(
        serde_json::to_string(&request).expect("serialize request"),
        r#"{"abi":1,"operation":{"kind":"entity_get","entity_type":"Temper.App.Task","entity_id":"task-1","at_least_sequence":7}}"#
    );
}

#[test]
fn unknown_fields_fail_closed() {
    let input = r#"{"abi":1,"operation":{"kind":"entity_get","entity_type":"Task","entity_id":"1","tenant":"other"}}"#;
    assert!(serde_json::from_str::<DataRequestV1>(input).is_err());
}

#[test]
fn exact_error_encoding() {
    let response = DataResponseV1::error(ModuleDataError::new(
        ModuleDataErrorKind::NotFound,
        "EntityNotFound",
        "entity not found",
        Retryability::Never,
    ));
    assert_eq!(
        serde_json::to_string(&response).expect("serialize response"),
        r#"{"abi":1,"outcome":{"kind":"error","error":{"kind":"not_found","code":"EntityNotFound","message":"entity not found","retryability":"never"}}}"#
    );
}
