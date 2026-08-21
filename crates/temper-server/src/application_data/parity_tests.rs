use std::collections::BTreeSet;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use temper_authz::SecurityContext;
use temper_wasm_sdk::data::{
    DataOperationKind, DataOperationV1, DataOutcomeV1, ModuleDataErrorKind,
};
use tower::ServiceExt;

use super::tests::{authenticated_router, call, invocation, response_error};

#[tokio::test]
async fn sdk_and_odata_share_authoritative_semantics() {
    let invocation = invocation(
        BTreeSet::from([
            DataOperationKind::EntityCreate,
            DataOperationKind::EntityGet,
            DataOperationKind::EntityPatch,
            DataOperationKind::ActionInvoke,
        ]),
        SecurityContext::system(),
    );
    let id = "018f1f80-7b2d-7000-8000-000000000001";
    let created = call(
        &invocation,
        DataOperationV1::EntityCreate {
            entity_type: "Temper.Example.Customer".into(),
            value: serde_json::json!({"Id": id, "Name": "Ada"})
                .as_object()
                .cloned()
                .unwrap(),
        },
    )
    .await;
    assert!(matches!(created.outcome, DataOutcomeV1::Ok { .. }));
    let path = format!("/tdata/Customers('{id}')");
    let router = authenticated_router(invocation.state.clone(), SecurityContext::system());
    assert_eq!(
        router
            .clone()
            .oneshot(Request::get(&path).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let patched = router
        .oneshot(
            Request::builder()
                .method(Method::PATCH)
                .uri(&path)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"Name":"Grace"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(patched.status(), StatusCode::OK);
    let sdk_read = call(
        &invocation,
        DataOperationV1::EntityGet {
            entity_type: "Temper.Example.Customer".into(),
            entity_id: id.into(),
            at_least_sequence: None,
        },
    )
    .await;
    let DataOutcomeV1::Ok {
        result: temper_wasm_sdk::data::DataResultV1::Entity { value, .. },
    } = sdk_read.outcome
    else {
        panic!("SDK read should see OData patch")
    };
    assert_eq!(value.get("Name"), Some(&serde_json::json!("Grace")));
    let action = call(
        &invocation,
        DataOperationV1::ActionInvoke {
            entity_type: "Temper.Example.Customer".into(),
            entity_id: id.into(),
            action: "Rename".into(),
            expected_sequence: None,
            params: serde_json::json!({"Name": "Ada"})
                .as_object()
                .cloned()
                .unwrap(),
        },
    )
    .await;
    assert!(
        matches!(action.outcome, DataOutcomeV1::Ok { .. }),
        "{action:?}"
    );
    let odata_action = authenticated_router(invocation.state.clone(), SecurityContext::system())
        .oneshot(
            Request::post(format!("{path}/Temper.Example.Rename"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"Name":"Grace"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(odata_action.status(), StatusCode::OK);
    let guard = call(
        &invocation,
        DataOperationV1::ActionInvoke {
            entity_type: "Temper.Example.Customer".into(),
            entity_id: id.into(),
            action: "Reject".into(),
            expected_sequence: None,
            params: Default::default(),
        },
    )
    .await;
    assert_eq!(
        response_error(guard).kind,
        ModuleDataErrorKind::GuardRejected
    );
    let odata_guard = authenticated_router(invocation.state.clone(), SecurityContext::system())
        .oneshot(
            Request::post(format!("{path}/Temper.Example.Reject"))
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(odata_guard.status(), StatusCode::CONFLICT);
    let missing = authenticated_router(invocation.state.clone(), SecurityContext::system())
        .oneshot(
            Request::get("/tdata/Customers('018f1f80-7b2d-7000-8000-000000000099')")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}
