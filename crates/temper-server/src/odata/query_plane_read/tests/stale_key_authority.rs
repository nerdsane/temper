use super::*;
use temper_runtime::persistence::{EntityKeyRow, EventStore};
use temper_store_sim::{SimEventStore, SimFaultConfig};

fn ws_path_filter(workspace_id: &str, path: &str) -> FilterExpr {
    let equality = |property: &str, value: &str| FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property(property.to_string())),
        op: BinaryOperator::Eq,
        right: Box::new(FilterExpr::Literal(ODataValue::String(value.to_string()))),
    };
    FilterExpr::BinaryOp {
        left: Box::new(equality("WorkspaceId", workspace_id)),
        op: BinaryOperator::And,
        right: Box::new(equality("Path", path)),
    }
}

fn ws_path_hash(workspace_id: &str, path: &str) -> String {
    crate::key_index::canonical_key_hash(
        "ws_path",
        &["WorkspaceId".to_string(), "Path".to_string()],
        &serde_json::Map::from_iter([
            ("WorkspaceId".to_string(), serde_json::json!(workspace_id)),
            ("Path".to_string(), serde_json::json!(path)),
        ]),
    )
    .expect("complete key hashes")
}

#[tokio::test]
async fn legacy_watermark_declines_stale_phantom_hit_for_live_entity() {
    let store = SimEventStore::new(0, SimFaultConfig::none());
    let mut state = build_order_state("stale-key-hit");
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    let tenant = TenantId::default();
    let agent_context = AgentContext::for_service("stale-key-hit-test");
    state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "live",
            "Create",
            serde_json::json!({"WorkspaceId": "ws", "Path": "/same"}),
            &agent_context,
        )
        .await
        .expect("create live matching entity");
    let mut live_state = state
        .get_tenant_entity_state(&tenant, "Order", "live")
        .await
        .expect("read live entity")
        .state;
    live_state.fields = serde_json::json!({
        "Id": "live",
        "WorkspaceId": "ws",
        "Path": "/same",
    });
    store
        .save_snapshot(
            &format!("{tenant}:Order:live"),
            live_state.sequence_nr,
            &serde_json::to_vec(&live_state).expect("serialize keyed legacy snapshot"),
        )
        .await
        .expect("persist keyed legacy snapshot");
    state.stop_and_remove_entity(&tenant, "Order", "live");
    let recovered = state
        .get_tenant_entity_state(&tenant, "Order", "live")
        .await
        .expect("recover live matching entity");
    assert_eq!(recovered.state.fields["WorkspaceId"], "ws");
    assert_eq!(recovered.state.fields["Path"], "/same");

    let key_hash = ws_path_hash("ws", "/same");
    store
        .backfill_entity_keys(tenant.as_str(), "Order", "live", &[])
        .await
        .expect("remove live row to reproduce pre-cutover projection");
    store
        .backfill_entity_keys(
            tenant.as_str(),
            "Order",
            "phantom",
            &[EntityKeyRow {
                key_name: "ws_path".to_string(),
                key_hash: key_hash.clone(),
            }],
        )
        .await
        .expect("seed stale phantom holder");
    store
        .mark_key_index_backfilled(tenant.as_str(), "Order", "ws_path")
        .await
        .expect("seed legacy watermark");
    assert_eq!(
        store
            .lookup_by_key(tenant.as_str(), "Order", "ws_path", &key_hash)
            .await
            .expect("read stale row")
            .as_deref(),
        Some("phantom")
    );

    let security_context = SecurityContext::system();
    let query_options = QueryOptions {
        filter: Some(ws_path_filter("ws", "/same")),
        ..QueryOptions::default()
    };
    let request = QueryPlaneReadRequest {
        state: &state,
        tenant: &tenant,
        security_ctx: &security_context,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &query_options,
        budget: QueryPlaneReadBudget {
            default_page_size: 10,
            max_entities: 10,
        },
    };
    assert!(
        super::super::keyed_candidate_ids(&request).await.is_none(),
        "a legacy signature must decline a phantom positive hit and select scan fallback"
    );
}

#[tokio::test]
async fn authoritative_keyed_read_waits_through_exact_reconciliation() {
    let store = SimEventStore::new(0, SimFaultConfig::none());
    let mut state = build_order_state("key-read-reconciliation-fence");
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    let tenant = TenantId::default();
    let entity_id = "live";
    let key_hash = ws_path_hash("ws", "/same");

    store
        .backfill_entity_keys(
            tenant.as_str(),
            "Order",
            entity_id,
            &[EntityKeyRow {
                key_name: "ws_path".to_string(),
                key_hash: key_hash.clone(),
            }],
        )
        .await
        .expect("seed authoritative key row");
    state
        .mark_key_index_backfilled(
            &tenant,
            "Order",
            r#"v2|[["ws_path",["WorkspaceId","Path"]]]"#,
        )
        .await
        .expect("seed exact key watermark");

    let reconciliation = store
        .acquire_projection_reconciliation_fence(tenant.as_str(), "Order")
        .await
        .expect("acquire exact-reconciliation fence");
    store
        .backfill_entity_keys(tenant.as_str(), "Order", entity_id, &[])
        .await
        .expect("purge row inside exact reconciliation");

    let security_context = SecurityContext::system();
    let query_options = QueryOptions {
        filter: Some(ws_path_filter("ws", "/same")),
        ..QueryOptions::default()
    };
    let request = QueryPlaneReadRequest {
        state: &state,
        tenant: &tenant,
        security_ctx: &security_context,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &query_options,
        budget: QueryPlaneReadBudget {
            default_page_size: 10,
            max_entities: 10,
        },
    };
    let candidates = super::super::keyed_candidate_ids(&request);
    tokio::pin!(candidates);
    let remained_blocked = tokio::select! {
        biased;
        result = &mut candidates => panic!("keyed read observed the purge window: {result:?}"),
        _ = tokio::task::yield_now() => true,
    };
    assert!(remained_blocked);

    store
        .backfill_entity_keys(
            tenant.as_str(),
            "Order",
            entity_id,
            &[EntityKeyRow {
                key_name: "ws_path".to_string(),
                key_hash,
            }],
        )
        .await
        .expect("restore exact key row");
    drop(reconciliation);

    assert_eq!(
        candidates.await,
        Some(vec![entity_id.to_string()]),
        "the indexed reader must observe the final reconciled generation"
    );
}
