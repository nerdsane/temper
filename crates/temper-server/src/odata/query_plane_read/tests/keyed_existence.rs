//! ARN-68 / ADR-0153 — the declared-key existence oracle: registry-aware key
//! resolution (A), authoritative backfill (B), and watermark-gated authoritative
//! absence (C). Exercised against the sim store, the DST-canonical backend that
//! co-commits key rows and maintains the watermark exactly like prod Postgres.

use super::*;
use temper_runtime::persistence::EventStore;
use temper_store_sim::{SimEventStore, SimFaultConfig};

#[test]
fn declared_keys_resolve_from_registry_not_just_transition_tables() {
    // ARN-68 root cause: runtime-installed os-app entities (File, Directory, …)
    // are registered in the per-tenant SpecRegistry, NOT in `state.transition_tables`
    // (which is only ever set by `with_specs` at boot). The keyed read fast path
    // must resolve declared keys from the registry — reading `transition_tables`
    // alone returns nothing, silently disabling the keyed path so every point read
    // scans and 413s at scale.
    let state = build_order_state("declared-keys-registry");
    let tenant = TenantId::default();

    // Precondition that reproduces the bug: Order is registry-only.
    assert!(
        state.transition_tables.get("Order").is_none(),
        "Order is registered in the registry, not transition_tables (the openpaw case)"
    );

    // The fix: declared_keys_for resolves it via the registry.
    let keys = state.declared_keys_for(&tenant, "Order");
    assert_eq!(
        keys.len(),
        1,
        "the declared [[key]] must be found via the registry"
    );
    assert_eq!(keys[0].name, "ws_path");
    assert_eq!(
        keys[0].properties,
        vec!["WorkspaceId".to_string(), "Path".to_string()]
    );

    // And an unregistered type still yields no keys (no false positives).
    assert!(state.declared_keys_for(&tenant, "Nonexistent").is_empty());
}

/// Build an Order state backed by the in-memory sim store — the DST-canonical store
/// that co-commits key rows AND maintains the backfill watermark (ADR-0153), so it is
/// a *sound* keyed backend (unlike Turso, which does not co-commit keys). Returns the
/// store handle so a test can assert on `entity_key_index`/watermark state directly.
fn build_order_state_with_sim(system_name: &str) -> (ServerState, SimEventStore) {
    let store = SimEventStore::new(0, SimFaultConfig::none());
    let mut state = build_order_state(system_name);
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    (state, store)
}

/// `WorkspaceId eq <ws> and Path eq <path>` — the shape that resolves to Order's
/// declared `ws_path` key.
fn ws_path_filter(ws: &str, path: &str) -> FilterExpr {
    let eq = |prop: &str, val: &str| FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property(prop.to_string())),
        op: BinaryOperator::Eq,
        right: Box::new(FilterExpr::Literal(ODataValue::String(val.to_string()))),
    };
    FilterExpr::BinaryOp {
        left: Box::new(eq("WorkspaceId", ws)),
        op: BinaryOperator::And,
        right: Box::new(eq("Path", path)),
    }
}

fn ws_path_hash(ws: &str, path: &str) -> String {
    let mut fields = serde_json::Map::new();
    fields.insert("WorkspaceId".to_string(), serde_json::json!(ws));
    fields.insert("Path".to_string(), serde_json::json!(path));
    crate::key_index::canonical_key_hash(
        "ws_path",
        &["WorkspaceId".to_string(), "Path".to_string()],
        &fields,
    )
    .expect("both key components present")
}

/// B (ADR-0153): the backfill must key EXISTING entities by enumerating the durable
/// store, not the lazy in-memory `entity_index`. We create keyed orders, then clear
/// the in-memory index to simulate a fresh boot (the OLD backfill enumerated that and
/// would key nothing), and prove the backfill still keys every entity and watermarks
/// the type.
#[tokio::test]
async fn key_index_backfill_keys_store_entities_absent_from_the_lazy_index() {
    let (state, store) = build_order_state_with_sim("key-backfill");
    let tenant = TenantId::default();
    let agent_ctx = AgentContext::for_service("key-backfill-test");
    let entities = [("ord-key-0", "ws1", "/a"), ("ord-key-1", "ws1", "/b")];
    for (eid, ws, path) in entities {
        // A Create event makes the entity enumerable by the durable store scan…
        state
            .dispatch_tenant_action(
                &tenant,
                "Order",
                eid,
                "Create",
                serde_json::json!({}),
                &agent_ctx,
            )
            .await
            .expect("create order");
        // …and a snapshot carries its key-valued fields (an entity that existed with
        // these fields before the [[key]] was declared — the backfill's target).
        let snapshot = serde_json::json!({
            "entity_type": "Order",
            "entity_id": eid,
            "status": "Draft",
            "item_count": 0,
            "fields": { "Id": eid, "Status": "Draft", "WorkspaceId": ws, "Path": path },
        });
        store
            .save_snapshot(
                &format!("{tenant}:Order:{eid}"),
                1,
                &serde_json::to_vec(&snapshot).unwrap(),
            )
            .await
            .expect("seed snapshot");
    }

    // Nothing is keyed yet: the entities were seeded via `save_snapshot` with no
    // key-bearing Create event, so the sim store's live co-commit saw no key fields.
    // The keyed fields exist only in the snapshot — exactly the "pre-existing entity
    // the backfill must key" case.
    assert!(
        store
            .lookup_by_key(
                tenant.as_str(),
                "Order",
                "ws_path",
                &ws_path_hash("ws1", "/a")
            )
            .await
            .unwrap()
            .is_none(),
        "precondition: not keyed before backfill"
    );

    // Fresh-boot simulation: the lazy index is empty. The pre-fix backfill read this.
    state.entity_index.write().unwrap().clear();
    state.entity_index_hydrated.write().unwrap().clear();
    assert!(state.list_entity_ids(&tenant, "Order").is_empty());

    state.populate_key_index_from_snapshots(&tenant).await;

    // Enumerated from the store and keyed both entities, and watermarked the type.
    assert!(
        state.key_index_backfill_complete(&tenant, "Order").await,
        "Order must be watermarked after a clean backfill"
    );
    for (ws, path) in [("ws1", "/a"), ("ws1", "/b")] {
        assert!(
            store
                .lookup_by_key(tenant.as_str(), "Order", "ws_path", &ws_path_hash(ws, path))
                .await
                .unwrap()
                .is_some(),
            "backfill must key {ws}{path}"
        );
    }
}

/// Robustness (ADR-0153): the backfill is RESUMABLE — already-keyed entities are
/// skipped (not re-loaded), so a re-run after a partial pass only processes the
/// remainder instead of re-loading all N. Pre-key one entity directly, then run the
/// backfill, and confirm it completes + watermarks with both entities keyed.
#[tokio::test]
async fn key_index_backfill_skips_already_keyed_entities_and_still_watermarks() {
    let (state, store) = build_order_state_with_sim("key-backfill-resume");
    let tenant = TenantId::default();
    let agent_ctx = AgentContext::for_service("resume-test");
    for (eid, ws, path) in [("ord-a", "ws1", "/a"), ("ord-b", "ws1", "/b")] {
        state
            .dispatch_tenant_action(
                &tenant,
                "Order",
                eid,
                "Create",
                serde_json::json!({}),
                &agent_ctx,
            )
            .await
            .expect("create");
        let snap = serde_json::json!({
            "entity_type": "Order", "entity_id": eid, "status": "Draft", "item_count": 0,
            "fields": { "Id": eid, "WorkspaceId": ws, "Path": path },
        });
        store
            .save_snapshot(
                &format!("{tenant}:Order:{eid}"),
                1,
                &serde_json::to_vec(&snap).unwrap(),
            )
            .await
            .expect("snap");
    }
    // Pre-key ord-a directly (a prior partial pass / co-commit already keyed it).
    store
        .backfill_entity_keys(
            tenant.as_str(),
            "Order",
            "ord-a",
            &[temper_runtime::persistence::EntityKeyRow {
                key_name: "ws_path".to_string(),
                key_hash: ws_path_hash("ws1", "/a"),
            }],
        )
        .await
        .expect("pre-key");

    state.populate_key_index_from_snapshots(&tenant).await;

    // ord-a was skipped via the already-keyed set; ord-b keyed fresh; type watermarked.
    assert!(state.key_index_backfill_complete(&tenant, "Order").await);
    for (ws, path) in [("ws1", "/a"), ("ws1", "/b")] {
        assert!(
            store
                .lookup_by_key(tenant.as_str(), "Order", "ws_path", &ws_path_hash(ws, path))
                .await
                .unwrap()
                .is_some()
        );
    }
}

/// Soundness (ADR-0153): a DELETED entity is correctly skipped (not keyed) and does
/// NOT block the watermark — only entities that exist-but-cannot-load do. A deleted
/// entity alongside a live one: the type still watermarks, the live one is keyed, the
/// deleted one is not.
#[tokio::test]
async fn key_index_backfill_skips_deleted_entities_without_blocking_watermark() {
    let (state, store) = build_order_state_with_sim("key-backfill-deleted");
    let tenant = TenantId::default();
    let agent_ctx = AgentContext::for_service("deleted-test");
    for (eid, status, path) in [
        ("ord-live", "Draft", "/live"),
        ("ord-del", "Deleted", "/del"),
    ] {
        state
            .dispatch_tenant_action(
                &tenant,
                "Order",
                eid,
                "Create",
                serde_json::json!({}),
                &agent_ctx,
            )
            .await
            .expect("create");
        let snap = serde_json::json!({
            "entity_type": "Order", "entity_id": eid, "status": status, "item_count": 0,
            "fields": { "Id": eid, "WorkspaceId": "ws1", "Path": path },
        });
        store
            .save_snapshot(
                &format!("{tenant}:Order:{eid}"),
                1,
                &serde_json::to_vec(&snap).unwrap(),
            )
            .await
            .expect("snap");
    }

    state.populate_key_index_from_snapshots(&tenant).await;

    assert!(
        state.key_index_backfill_complete(&tenant, "Order").await,
        "a deleted entity must not block the watermark"
    );
    assert!(
        store
            .lookup_by_key(
                tenant.as_str(),
                "Order",
                "ws_path",
                &ws_path_hash("ws1", "/live")
            )
            .await
            .unwrap()
            .is_some(),
        "live entity is keyed"
    );
    assert!(
        store
            .lookup_by_key(
                tenant.as_str(),
                "Order",
                "ws_path",
                &ws_path_hash("ws1", "/del")
            )
            .await
            .unwrap()
            .is_none(),
        "deleted entity is not keyed"
    );
}

/// Soundness gate (ADR-0153): an entity that EXISTS but whose journal cannot be read
/// is classified `LoadFailed` — it must NOT be keyed AND must block the watermark
/// (otherwise a keyed miss for it would wrongly read as authoritative absence). The
/// backfill then resumes on a later run once the read succeeds. Without this, a
/// transient journal-read error during backfill would silently produce a permanent
/// wrong-absent.
#[tokio::test]
async fn key_index_backfill_loadfailed_entity_blocks_watermark_then_resumes() {
    let (state, store) = build_order_state_with_sim("key-backfill-loadfail");
    let tenant = TenantId::default();
    let agent_ctx = AgentContext::for_service("loadfail-test");
    let pid = format!("{tenant}:Order:ord-x");
    state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "ord-x",
            "Create",
            serde_json::json!({}),
            &agent_ctx,
        )
        .await
        .expect("create");
    let snap = serde_json::json!({
        "entity_type": "Order", "entity_id": "ord-x", "status": "Draft", "item_count": 0,
        "total_event_count": 1,
        "fields": { "Id": "ord-x", "WorkspaceId": "ws1", "Path": "/x" },
    });
    store
        .save_snapshot(&pid, 1, &serde_json::to_vec(&snap).unwrap())
        .await
        .expect("snap");

    // Run 1: the entity's journal read fails → LoadFailed → type NOT watermarked,
    // entity NOT keyed.
    store.fail_next_reads(&pid, 1);
    state.populate_key_index_from_snapshots(&tenant).await;
    assert!(
        !state.key_index_backfill_complete(&tenant, "Order").await,
        "an unloadable entity must block the watermark"
    );
    assert!(
        store
            .lookup_by_key(
                tenant.as_str(),
                "Order",
                "ws_path",
                &ws_path_hash("ws1", "/x")
            )
            .await
            .unwrap()
            .is_none(),
        "the unloadable entity must not be keyed"
    );

    // Run 2 (resume): the read now succeeds → entity keyed → type watermarked.
    state.populate_key_index_from_snapshots(&tenant).await;
    assert!(
        state.key_index_backfill_complete(&tenant, "Order").await,
        "backfill must resume and watermark once the read succeeds"
    );
    assert!(
        store
            .lookup_by_key(
                tenant.as_str(),
                "Order",
                "ws_path",
                &ws_path_hash("ws1", "/x")
            )
            .await
            .unwrap()
            .is_some(),
        "the entity is keyed on resume"
    );
}

/// C (ADR-0153): once the backfill watermark is set, a keyed read MISS is
/// authoritative absence — the read returns empty WITHOUT the full-type scan that
/// otherwise 413s at scale (ARN-68). Before the watermark, the same miss falls back
/// to the scan and 413s. This is the end-to-end proof that the fix removes the 413.
#[tokio::test]
async fn keyed_miss_returns_empty_without_scan_413_once_watermarked() {
    let (state, _store) = build_order_state_with_sim("keyed-absence");
    let tenant = TenantId::default();
    let security_ctx = SecurityContext::system();
    // More orders than the scan budget (max_entities=1 → scan_candidate_budget=10),
    // so the fallback scan would trip the budget.
    create_orders(&state, 11).await;

    let query_options = QueryOptions {
        // Resolves to ws_path but matches no entity (a genuine miss).
        filter: Some(ws_path_filter("nope", "/none")),
        ..QueryOptions::default()
    };
    let budget = QueryPlaneReadBudget {
        default_page_size: 1,
        max_entities: 1,
    };

    // Before the watermark: keyed miss → scan fallback → 413.
    match read_entity_set_from_query_plane(QueryPlaneReadRequest {
        state: &state,
        tenant: &tenant,
        security_ctx: &security_ctx,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &query_options,
        budget,
    })
    .await
    {
        Err(QueryPlaneReadError::QueryTooLarge { .. }) => {}
        Ok(_) => panic!("expected 413 before watermark, got Ok"),
        Err(_) => panic!("expected QueryTooLarge before watermark, got another error"),
    }

    // Watermark Order → a keyed miss is now authoritative absence.
    state.mark_key_index_backfilled(&tenant, "Order").await;

    let result = match read_entity_set_from_query_plane(QueryPlaneReadRequest {
        state: &state,
        tenant: &tenant,
        security_ctx: &security_ctx,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &query_options,
        budget,
    })
    .await
    {
        Ok(result) => result,
        Err(_) => panic!("no 413 once watermarked — keyed miss must be authoritative absence"),
    };
    assert!(result.entities.is_empty(), "a genuine miss returns no rows");
    assert_eq!(
        result.telemetry.fallback_reason,
        QueryPlaneFallbackReason::KeyedAbsence,
        "the read must resolve via keyed absence, not a scan"
    );
}
