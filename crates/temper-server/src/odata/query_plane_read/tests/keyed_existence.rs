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
