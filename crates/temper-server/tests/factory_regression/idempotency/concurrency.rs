use super::*;

// ---------------------------------------------------------------------------
// Concurrency (warm).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_identical_requests_append_one_event() {
    let (_g, _c, _i) = install_deterministic_context(5251);
    let h = Harness::new(5251);
    let id = "order-concurrent-same";
    h.create_order(TENANT_A, id).await;
    let before = h.journal_len(TENANT_A, id);

    let (a, b) = tokio::join!(
        h.action(TENANT_A, id, "Temper.Example.AddItem", Some(K1), ORIGINAL),
        h.send(action_request(
            TENANT_A,
            id,
            "Temper.AddItem",
            Some(K1),
            r#"{"Meta":{"tags":["a","b"],"gift":{"note":"n","wrap":true}},"Quantity":1,"ProductId":"p-1"}"#,
            TRANSPORT_HEADERS,
            Some(super::harness::TESTER),
        )),
    );
    assert_eq!(
        a.status,
        StatusCode::OK,
        "first concurrent caller: {}",
        a.body
    );
    assert_eq!(
        b.status,
        StatusCode::OK,
        "second concurrent caller: {}",
        b.body
    );
    assert_eq!(
        logical(&a.body),
        logical(&b.body),
        "concurrent identical callers saw different responses"
    );
    assert_eq!(
        h.journal_len(TENANT_A, id),
        before + 1,
        "concurrent identical callers must append exactly one event"
    );
}

#[tokio::test]
async fn concurrent_different_bodies_one_wins() {
    let (_g, _c, _i) = install_deterministic_context(5261);
    let h = Harness::new(5261);
    let id = "order-concurrent-diff";
    h.create_order(TENANT_A, id).await;
    let before = h.journal_len(TENANT_A, id);

    let (a, b) = tokio::join!(
        h.action(TENANT_A, id, "Temper.Example.AddItem", Some(K1), ORIGINAL),
        h.action(
            TENANT_A,
            id,
            "Temper.Example.AddItem",
            Some(K1),
            OTHER_QUANTITY
        ),
    );
    let ok = [&a, &b]
        .iter()
        .filter(|r| r.status == StatusCode::OK)
        .count();
    let mismatched = [&a, &b]
        .iter()
        .filter(|r| {
            r.status == StatusCode::UNPROCESSABLE_ENTITY
                && r.error_code() == Some("IdempotencyKeyMismatch")
        })
        .count();
    assert!(
        ok == 1 && mismatched == 1,
        "{MARK}: concurrent callers with one key and different bodies: expected one 200 and one \
         422 IdempotencyKeyMismatch, got {} {} and {} {}",
        a.status,
        a.body,
        b.status,
        b.body
    );
    assert_eq!(
        h.journal_len(TENANT_A, id),
        before + 1,
        "{MARK}: concurrent different-body callers must append exactly one event"
    );
}
