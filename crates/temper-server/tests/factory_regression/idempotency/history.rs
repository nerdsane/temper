use super::*;

// ---------------------------------------------------------------------------
// Legacy (pre-binding) journal and snapshot.
// ---------------------------------------------------------------------------

/// Run K1 + K2 in a current-format store, then rebuild a legacy store (no
/// binding field, old-format snapshot at the head) and boot over it cold.
async fn legacy_setup(seed: u64, extra_processed_keys: &[&str]) -> (Harness, Reply, usize) {
    let h = Harness::new(seed);
    h.create_order(TENANT_A, ORDER).await;
    let original = h
        .action(
            TENANT_A,
            ORDER,
            "Temper.Example.AddItem",
            Some(K1),
            ORIGINAL,
        )
        .await;
    assert_eq!(
        original.status,
        StatusCode::OK,
        "harness setup: original K1 failed: {}",
        original.body
    );
    let second = h
        .action(TENANT_A, ORDER, "Temper.Example.AddItem", Some(K2), SECOND)
        .await;
    assert_eq!(
        second.status,
        StatusCode::OK,
        "harness setup: K2 failed: {}",
        second.body
    );

    let legacy = legacy_copy(&h.store, TENANT_A, ORDER, seed + 1).await;
    save_legacy_snapshot(&legacy, TENANT_A, ORDER, &second.body, extra_processed_keys).await;
    let cold = Harness::over(legacy);
    let len = cold.journal_len(TENANT_A, ORDER);
    (cold, original, len)
}

#[tokio::test]
async fn legacy_unscoped_journal_and_snapshot_same_request_is_unverifiable() {
    let (_g, _c, _i) = install_deterministic_context(5271);
    let (h, _original, before) = legacy_setup(5271, &[]).await;
    let retry = h
        .action(TENANT_A, ORDER, "Temper.AddItem", Some(K1), EQUIVALENT)
        .await;
    assert_eq!(
        retry.status,
        StatusCode::CONFLICT,
        "legacy reply has no authority proof: {}",
        retry.body
    );
    assert_eq!(retry.error_code(), Some("IdempotencyKeyUnverifiable"));
    assert_eq!(
        h.journal_len(TENANT_A, ORDER),
        before,
        "{MARK}: legacy journal+snapshot: idempotent retry appended an event"
    );
}

#[tokio::test]
async fn legacy_journal_different_body_is_rejected() {
    let (_g, _c, _i) = install_deterministic_context(5281);
    let (h, _original, before) = legacy_setup(5281, &[]).await;
    let reply = h
        .action(
            TENANT_A,
            ORDER,
            "Temper.Example.AddItem",
            Some(K1),
            OTHER_QUANTITY,
        )
        .await;
    let after = h.journal_len(TENANT_A, ORDER);
    assert_rejected(
        &reply,
        StatusCode::UNPROCESSABLE_ENTITY,
        "IdempotencyKeyMismatch",
        before,
        after,
        "legacy journal: different body",
    );
}

#[tokio::test]
async fn legacy_snapshot_key_without_event_fails_closed() {
    let (_g, _c, _i) = install_deterministic_context(5291);
    let ghost = "idem-ghost";
    let (h, _original, before) = legacy_setup(5291, &[ghost]).await;
    let reply = h
        .action(
            TENANT_A,
            ORDER,
            "Temper.Example.AddItem",
            Some(ghost),
            SECOND,
        )
        .await;
    assert!(
        reply.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("original request cannot be verified")
    );
    let after = h.journal_len(TENANT_A, ORDER);
    assert_rejected(
        &reply,
        StatusCode::CONFLICT,
        "IdempotencyKeyUnverifiable",
        before,
        after,
        "legacy snapshot key with no journal event",
    );
}

// ---------------------------------------------------------------------------
// Tenant isolation and fail-closed auth (guard).
// ---------------------------------------------------------------------------

async fn binding_respects_tenant_and_auth(mode: Mode, seed: u64) {
    let (_g, _c, _i) = install_deterministic_context(seed);
    let (mut h, _original, before) = setup(seed).await;
    h.create_order(TENANT_B, ORDER).await;
    let b_before = h.journal_len(TENANT_B, ORDER);
    h.maybe_restart(mode);

    // Same id + key in another tenant executes independently.
    let b = h
        .action(
            TENANT_B,
            ORDER,
            "Temper.Example.AddItem",
            Some(K1),
            OTHER_QUANTITY,
        )
        .await;
    assert_eq!(
        b.status,
        StatusCode::OK,
        "{mode:?}: tenant B must execute independently: {}",
        b.body
    );
    assert_eq!(
        h.journal_len(TENANT_B, ORDER),
        b_before + 1,
        "{mode:?}: tenant B event missing"
    );
    assert_eq!(
        h.journal_len(TENANT_A, ORDER),
        before,
        "{mode:?}: tenant B wrote tenant A's journal"
    );

    // Denied principal never sees a cached hit.
    let denied = h
        .send(action_request(
            TENANT_A,
            ORDER,
            "Temper.Example.AddItem",
            Some(K1),
            ORIGINAL,
            &[],
            Some(INTRUDER),
        ))
        .await;
    assert_eq!(
        denied.status,
        StatusCode::FORBIDDEN,
        "{mode:?}: denied principal: {}",
        denied.body
    );
    assert!(
        status(&denied.body).is_none(),
        "{mode:?}: denied response leaked entity state: {}",
        denied.body
    );

    // Unauthenticated request is rejected before any cache lookup.
    let anonymous = h
        .send(action_request(
            TENANT_A,
            ORDER,
            "Temper.Example.AddItem",
            Some(K1),
            ORIGINAL,
            &[],
            None,
        ))
        .await;
    assert_eq!(
        anonymous.status,
        StatusCode::UNAUTHORIZED,
        "{mode:?}: unauthenticated: {}",
        anonymous.body
    );
    assert!(
        status(&anonymous.body).is_none(),
        "{mode:?}: unauthenticated response leaked entity state"
    );
    assert_eq!(
        h.journal_len(TENANT_A, ORDER),
        before,
        "{mode:?}: rejected requests appended events"
    );
}

#[tokio::test]
async fn binding_respects_tenant_and_auth_warm() {
    binding_respects_tenant_and_auth(Mode::WarmHttpCache, 5301).await;
}

#[tokio::test]
async fn binding_respects_tenant_and_auth_cold() {
    binding_respects_tenant_and_auth(Mode::ColdRestart, 5302).await;
}

// ---------------------------------------------------------------------------
// Offline replay fold vs response history, GET and catalog (guard).
// ---------------------------------------------------------------------------

/// Fold `(status, items)` from journal payloads, optionally stopping after the
/// first event carrying `stop_key`.
fn fold(
    journal: &[temper_runtime::persistence::PersistenceEnvelope],
    stop_key: Option<&str>,
) -> (String, u64) {
    let mut status = String::new();
    let mut items: u64 = 0;
    for env in journal {
        let p = &env.payload;
        if let Some(to) = p.get("to_status").and_then(Value::as_str) {
            status = to.to_string();
        }
        match p.get("action").and_then(Value::as_str) {
            Some("AddItem") => items += 1,
            Some("RemoveItem") => items = items.saturating_sub(1),
            _ => {}
        }
        if stop_key.is_some() && p.get("idempotency_key").and_then(Value::as_str) == stop_key {
            break;
        }
    }
    (status, items)
}

#[tokio::test]
async fn offline_fold_matches_history_catalog_and_get() {
    let (_g, _c, _i) = install_deterministic_context(5311);
    let (h, original, _before) = setup(5311).await;
    let journal = h.journal(TENANT_A, ORDER);

    let (full_status, full_items) = fold(&journal, None);
    let get = h.get_order(TENANT_A, ORDER).await;
    assert_eq!(get.status, StatusCode::OK, "GET order: {}", get.body);
    assert_eq!(
        status(&get.body),
        Some(full_status.as_str()),
        "fold status != GET: {}",
        get.body
    );
    assert_eq!(
        items(&get.body),
        Some(full_items),
        "fold items != GET: {}",
        get.body
    );

    let catalog = h.list_orders(TENANT_A).await;
    assert_eq!(catalog.status, StatusCode::OK, "catalog: {}", catalog.body);
    let row = catalog
        .body
        .get("value")
        .and_then(Value::as_array)
        .and_then(|rows| {
            rows.iter().find(|r| {
                r.get("entity_id").and_then(Value::as_str) == Some(ORDER)
                    || r.get("Id").and_then(Value::as_str) == Some(ORDER)
            })
        })
        .unwrap_or_else(|| panic!("order missing from catalog: {}", catalog.body));
    assert_eq!(
        status(row),
        Some(full_status.as_str()),
        "fold status != catalog row: {row}"
    );

    let (k1_status, k1_items) = fold(&journal, Some(K1));
    assert_eq!(
        status(&original.body),
        Some(k1_status.as_str()),
        "prefix fold status != original K1 response"
    );
    assert_eq!(
        items(&original.body),
        Some(k1_items),
        "prefix fold items != original K1 response"
    );
}
