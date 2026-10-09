use super::*;

// ---------------------------------------------------------------------------
// Same key, same logical request -> original response, no new event.
// ---------------------------------------------------------------------------

async fn same_logical_request_returns_original(mode: Mode, seed: u64) {
    let (_g, _c, _i) = install_deterministic_context(seed);
    let (mut h, original, before) = setup(seed).await;
    h.maybe_restart(mode);

    // Different qualifier, reordered nested keys, top-level Id, transport headers.
    let retry = h
        .send(action_request(
            TENANT_A,
            ORDER,
            "Temper.AddItem",
            Some(K1),
            EQUIVALENT,
            TRANSPORT_HEADERS,
            Some(super::harness::TESTER),
        ))
        .await;
    if mode == Mode::ColdRestart {
        // Approved compatibility change: no durable final dispatcher reply.
        assert_eq!(retry.status, StatusCode::CONFLICT);
        assert_eq!(retry.error_code(), Some("IdempotencyKeyUnverifiable"));
    } else {
        assert!(
            retry.status == StatusCode::OK && logical(&retry.body) == logical(&original.body),
            "{MARK}: {mode:?}: same-key same-logical retry after an intervening action must return \
         the ORIGINAL response.\n status: {}\n original: {}\n got: {}",
            retry.status,
            logical(&original.body),
            logical(&retry.body)
        );
    }
    assert_eq!(
        h.journal_len(TENANT_A, ORDER),
        before,
        "{MARK}: {mode:?}: idempotent retry appended a journal event"
    );
}

#[tokio::test]
async fn same_logical_request_returns_original_response_after_intervening_action_warm() {
    same_logical_request_returns_original(Mode::WarmHttpCache, 5191).await;
}

#[tokio::test]
async fn cold_dispatcher_reply_without_completion_proof_is_unverifiable() {
    same_logical_request_returns_original(Mode::ColdRestart, 5192).await;
}

// ---------------------------------------------------------------------------
// Same key, different action or body -> 422, no event.
// ---------------------------------------------------------------------------

async fn mismatch_is_rejected(
    mode: Mode,
    seed: u64,
    qualified_action: &str,
    body: &str,
    scenario: &str,
) {
    let (_g, _c, _i) = install_deterministic_context(seed);
    let (mut h, _original, before) = setup(seed).await;
    let status_before = h.get_order(TENANT_A, ORDER).await;
    h.maybe_restart(mode);

    let reply = h
        .action(TENANT_A, ORDER, qualified_action, Some(K1), body)
        .await;
    let after = h.journal_len(TENANT_A, ORDER);
    assert_rejected(
        &reply,
        StatusCode::UNPROCESSABLE_ENTITY,
        "IdempotencyKeyMismatch",
        before,
        after,
        &format!("{mode:?}: {scenario}"),
    );
    let get = h.get_order(TENANT_A, ORDER).await;
    assert_eq!(
        status(&get.body),
        status(&status_before.body),
        "{MARK}: {mode:?}: {scenario}: mismatched request changed entity status"
    );
}

const CANCEL: &str = r#"{"Reason":"changed my mind"}"#;
const REORDERED_ARRAY: &str =
    r#"{"ProductId":"p-1","Quantity":1,"Meta":{"gift":{"wrap":true,"note":"n"},"tags":["b","a"]}}"#;
const NESTED_ODATA_PROPERTY: &str = r##"{"ProductId":"p-1","Quantity":1,"Meta":{"gift":{"wrap":true,"note":"n","@odata.type":"#Gift"},"tags":["a","b"]}}"##;

#[tokio::test]
async fn different_action_same_key_is_rejected_warm() {
    mismatch_is_rejected(
        Mode::WarmHttpCache,
        5201,
        "Temper.Example.CancelOrder",
        CANCEL,
        "different action",
    )
    .await;
}

#[tokio::test]
async fn different_action_same_key_is_rejected_cold() {
    mismatch_is_rejected(
        Mode::ColdRestart,
        5202,
        "Temper.Example.CancelOrder",
        CANCEL,
        "different action",
    )
    .await;
}

#[tokio::test]
async fn different_body_same_key_is_rejected_warm() {
    mismatch_is_rejected(
        Mode::WarmHttpCache,
        5211,
        "Temper.Example.AddItem",
        OTHER_QUANTITY,
        "different body",
    )
    .await;
}

#[tokio::test]
async fn different_body_same_key_is_rejected_cold() {
    mismatch_is_rejected(
        Mode::ColdRestart,
        5212,
        "Temper.Example.AddItem",
        OTHER_QUANTITY,
        "different body",
    )
    .await;
}

#[tokio::test]
async fn nested_array_order_is_significant_warm() {
    mismatch_is_rejected(
        Mode::WarmHttpCache,
        5221,
        "Temper.Example.AddItem",
        REORDERED_ARRAY,
        "nested array reordered",
    )
    .await;
}

#[tokio::test]
async fn nested_array_order_is_significant_cold() {
    mismatch_is_rejected(
        Mode::ColdRestart,
        5222,
        "Temper.Example.AddItem",
        REORDERED_ARRAY,
        "nested array reordered",
    )
    .await;
}

#[tokio::test]
async fn nested_odata_named_business_property_is_bound_warm() {
    mismatch_is_rejected(
        Mode::WarmHttpCache,
        5231,
        "Temper.Example.AddItem",
        NESTED_ODATA_PROPERTY,
        "nested @odata.* business property added",
    )
    .await;
}

#[tokio::test]
async fn nested_odata_named_business_property_is_bound_cold() {
    mismatch_is_rejected(
        Mode::ColdRestart,
        5232,
        "Temper.Example.AddItem",
        NESTED_ODATA_PROPERTY,
        "nested @odata.* business property added",
    )
    .await;
}

// ---------------------------------------------------------------------------
// Failed requests never bind a key (guard).
// ---------------------------------------------------------------------------

async fn failed_request_never_binds_key(mode: Mode, seed: u64) {
    let (_g, _c, _i) = install_deterministic_context(seed);
    let mut h = Harness::new(seed);
    let id = "order-failed-bind";
    let key = "idem-failed";
    h.create_order(TENANT_A, id).await;
    let empty = h.journal_len(TENANT_A, id);

    // Guard `items > 0` fails on an empty order.
    let failed = h
        .action(
            TENANT_A,
            id,
            "Temper.Example.RemoveItem",
            Some(key),
            r#"{"ItemId":"i-1"}"#,
        )
        .await;
    assert_eq!(
        failed.status,
        StatusCode::CONFLICT,
        "failed RemoveItem should be 409: {}",
        failed.body
    );
    assert_eq!(
        h.journal_len(TENANT_A, id),
        empty,
        "failed request appended an event"
    );
    h.maybe_restart(mode);

    let applied = h
        .action(TENANT_A, id, "Temper.Example.AddItem", Some(key), SECOND)
        .await;
    assert_eq!(
        applied.status,
        StatusCode::OK,
        "{mode:?}: a key whose only use failed must stay free: {}",
        applied.body
    );
    assert_eq!(
        h.journal_len(TENANT_A, id),
        empty + 1,
        "{mode:?}: AddItem must append one event"
    );
    h.maybe_restart(mode);

    let retry = h
        .action(TENANT_A, id, "Temper.Example.AddItem", Some(key), SECOND)
        .await;
    if mode == Mode::ColdRestart {
        assert_eq!(retry.status, StatusCode::CONFLICT);
        assert_eq!(retry.error_code(), Some("IdempotencyKeyUnverifiable"));
    } else {
        assert_eq!(
            retry.status,
            StatusCode::OK,
            "{mode:?}: retry should succeed: {}",
            retry.body
        );
        assert_eq!(
            logical(&retry.body),
            logical(&applied.body),
            "{mode:?}: retry should return the original response"
        );
    }
    assert_eq!(
        h.journal_len(TENANT_A, id),
        empty + 1,
        "{mode:?}: retry appended an event"
    );
}

#[tokio::test]
async fn failed_request_never_binds_key_warm() {
    failed_request_never_binds_key(Mode::WarmHttpCache, 5241).await;
}

#[tokio::test]
async fn failed_request_never_binds_key_cold() {
    failed_request_never_binds_key(Mode::ColdRestart, 5242).await;
}
