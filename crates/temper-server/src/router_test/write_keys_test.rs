//! Every write's answer names its idempotency key, and the same write sent
//! again with that key is applied once and answered as the first time.

use super::*;

const ADD_ITEM: &str = "/tdata/Orders('keyed-1')/Temper.Example.AddItem";
const ADD_ITEM_BODY: &str = r#"{"ProductId":"6f9619ff-8b86-d011-b42d-00cf4fc964ff","Quantity":1}"#;

/// Send `AddItem`, with `key` if given; the answer's status, the key it
/// names and its body.
async fn add_item(
    app: &Router,
    key: Option<&str>,
) -> (StatusCode, Option<String>, serde_json::Value) {
    let mut request = Request::post(ADD_ITEM)
        .header("Content-Type", "application/json")
        .header("X-Temper-Principal-Kind", "admin");
    if let Some(key) = key {
        request = request.header("Idempotency-Key", key);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(ADD_ITEM_BODY)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let named = response
        .headers()
        .get("idempotency-key")
        .and_then(|key| key.to_str().ok())
        .map(str::to_string);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        named,
        serde_json::from_slice(&body).unwrap_or_default(),
    )
}

async fn items(app: &Router) -> serde_json::Value {
    let response = app
        .clone()
        .oneshot(
            Request::get("/tdata/Orders('keyed-1')")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let order: serde_json::Value = serde_json::from_slice(&body).unwrap();
    order["counters"]["items"].clone()
}

#[tokio::test]
async fn a_write_answer_names_its_key_and_a_resend_with_it_applies_once() {
    let app = authenticated_router(test_state_with_ioa());

    let (status, key, first) = add_item(&app, None).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let key = key.expect("the answer names the key the write was given");
    assert_eq!(items(&app).await, 1);

    // The caller never heard that answer (or heard "not confirmed") and sends
    // the same write again with its key.
    let (status, again_key, again) = add_item(&app, Some(&key)).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again_key.as_deref(), Some(key.as_str()));
    assert_eq!(
        again["counters"], first["counters"],
        "answered as the first time"
    );
    assert_eq!(items(&app).await, 1, "applied once");

    // A caller's own key is the one its answer names.
    let (status, own, _) = add_item(&app, Some("caller-key-1")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(own.as_deref(), Some("caller-key-1"));
    assert_eq!(items(&app).await, 2);

    // A refused write names its key too, so it can be sent again as itself.
    let response = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders('keyed-1')/Temper.Example.NoSuchAction")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Kind", "admin")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(!response.status().is_success(), "{}", response.status());
    assert!(response.headers().contains_key("idempotency-key"));
}

/// Create an order with `body`, with `key` if given; the answer's status,
/// the key it names and the record's id.
async fn create_order(
    app: &Router,
    key: Option<&str>,
    body: &str,
) -> (StatusCode, Option<String>, String) {
    let mut request = Request::post("/tdata/Orders").header("Content-Type", "application/json");
    if let Some(key) = key {
        request = request.header("Idempotency-Key", key);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let named = response
        .headers()
        .get("idempotency-key")
        .and_then(|key| key.to_str().ok())
        .map(str::to_string);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let record: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
    let id = record["entity_id"].as_str().unwrap_or_default().to_string();
    (status, named, id)
}

async fn order_count(app: &Router) -> usize {
    let response = app
        .clone()
        .oneshot(Request::get("/tdata/Orders").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let list: serde_json::Value = serde_json::from_slice(&body).unwrap();
    list["value"].as_array().map_or(0, Vec::len)
}

#[tokio::test]
async fn a_create_resent_with_its_key_gives_the_same_record() {
    let app = authenticated_router(test_state_with_ioa());

    // No id and no key: the answer names the key the create was given.
    let (status, key, first) = create_order(&app, None, "{}").await;
    assert_eq!(status, StatusCode::CREATED);
    let key = key.expect("the answer names the create's key");
    assert!(!first.is_empty());

    // Sent again with that key: the same record, still one.
    let (status, again_key, again) = create_order(&app, Some(&key), "{}").await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(again_key.as_deref(), Some(key.as_str()));
    assert_eq!(
        again, first,
        "the resend answers the record the create made"
    );
    assert_eq!(order_count(&app).await, 1);

    // A caller's own key, of any form, gives one record however often it is sent.
    let (_, _, own) = create_order(&app, Some("order from cart 7"), "{}").await;
    let (_, _, own_again) = create_order(&app, Some("order from cart 7"), "{}").await;
    assert_eq!(own, own_again);
    assert_ne!(own, first);
    assert_eq!(order_count(&app).await, 2);

    // A create that names its id keeps it.
    let (status, _, named) = create_order(&app, Some("k-named"), r#"{"id":"named-1"}"#).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(named, "named-1");
}
