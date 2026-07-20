use super::*;
use crate::entity_actor::{EntityEvent, recover_entity_state_from_store};

async fn read_path(
    state: &ServerState,
    tenant: &TenantId,
    workspace: &str,
    path: &str,
) -> Result<QueryPlaneReadResult, QueryPlaneReadError> {
    let options = QueryOptions {
        filter: Some(ws_path_filter(workspace, path)),
        ..QueryOptions::default()
    };
    let security_ctx = SecurityContext::system();
    read_entity_set_page(QueryPlaneReadRequest {
        state,
        tenant,
        security_ctx: &security_ctx,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &options,
        budget: QueryPlaneReadBudget {
            default_page_size: 10,
            max_entities: 10,
        },
    })
    .await
}

fn expect_read(
    result: Result<QueryPlaneReadResult, QueryPlaneReadError>,
    message: &str,
) -> QueryPlaneReadResult {
    match result {
        Ok(result) => result,
        Err(_) => panic!("{message}"),
    }
}

fn tombstone_event(persistence_id: &str) -> PersistenceEnvelope {
    let timestamp = sim_now();
    let event = EntityEvent {
        action: "Delete".to_string(),
        from_status: "Draft".to_string(),
        to_status: "Deleted".to_string(),
        timestamp,
        params: serde_json::json!({}),
        idempotency_key: Some("terminal-delete".to_string()),
    };
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: "Delete".to_string(),
        payload: serde_json::to_value(event).expect("serialize tombstone"),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp,
            actor_id: persistence_id.to_string(),
        },
    }
}

#[tokio::test]
async fn exhausted_journal_faults_return_unstable_not_authoritative_absence() {
    let (_guard, _clock, _ids) = install_deterministic_context(255);
    let tenant = TenantId::default();
    let workspace = "ws-replay-fault";
    let path = "/present";
    let entity_id = "ord-replay-fault";
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    let (state, store) = build_order_state_with_sim("incomplete-replay-fault");

    EventStore::append_with_index_rows(
        &store,
        &persistence_id,
        0,
        &[field_update_event(
            &persistence_id,
            path,
            "faulted-present-entity",
        )],
        &[key_row(workspace, path)],
        &[],
        IndexReconciliation {
            keys: true,
            key_set_signature: Some(ORDER_KEY_SET_SIGNATURE.to_string()),
            vectors: false,
        },
    )
    .await
    .expect("seed present journal entity");
    store.fail_next_reads(&persistence_id, 3);

    let result = read_path(&state, &tenant, workspace, path).await;
    assert!(
        matches!(result, Err(QueryPlaneReadError::KeyOwnershipUnstable)),
        "journal uncertainty must not be flattened into an empty successful read"
    );
}

#[tokio::test]
async fn incomplete_scan_keeps_tombstone_terminal_through_legacy_suffix() {
    let (_guard, _clock, _ids) = install_deterministic_context(256);
    let tenant = TenantId::default();
    let workspace = "ws-terminal-suffix";
    let path = "/deleted";
    let entity_id = "ord-terminal-suffix";
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    let (state, store) = build_order_state_with_sim("incomplete-terminal-suffix");

    EventStore::append_with_index_rows(
        &store,
        &persistence_id,
        0,
        &[field_update_event(
            &persistence_id,
            path,
            "terminal-live-generation",
        )],
        &[key_row(workspace, path)],
        &[],
        IndexReconciliation {
            keys: true,
            key_set_signature: Some(ORDER_KEY_SET_SIGNATURE.to_string()),
            vectors: false,
        },
    )
    .await
    .expect("seed live journal state");
    EventStore::append_with_index_rows(
        &store,
        &persistence_id,
        1,
        &[tombstone_event(&persistence_id)],
        &[],
        &[],
        IndexReconciliation {
            keys: true,
            key_set_signature: Some(ORDER_KEY_SET_SIGNATURE.to_string()),
            vectors: false,
        },
    )
    .await
    .expect("append terminal tombstone");
    EventStore::append_with_index_rows(
        &store,
        &persistence_id,
        2,
        &[field_update_event(
            &persistence_id,
            "/legacy-resurrection",
            "legacy-suffix",
        )],
        &[],
        &[],
        IndexReconciliation {
            keys: true,
            key_set_signature: Some(ORDER_KEY_SET_SIGNATURE.to_string()),
            vectors: false,
        },
    )
    .await
    .expect("append legacy suffix after tombstone");
    EventStore::save_snapshot(
        &store,
        &persistence_id,
        3,
        &snapshot(entity_id, workspace, path),
    )
    .await
    .expect("seed newer stale live snapshot");

    let result = expect_read(
        read_path(&state, &tenant, workspace, path).await,
        "terminal suffix scan must remain available",
    );
    assert!(
        result.entities.is_empty(),
        "the terminal tombstone must win and the full suffix high-water must be consumed"
    );
    let suffix = expect_read(
        read_path(&state, &tenant, workspace, "/legacy-resurrection").await,
        "legacy-suffix key scan must remain available",
    );
    assert!(
        suffix.entities.is_empty(),
        "a post-tombstone suffix must not resurrect ownership under its own key"
    );

    let table = {
        let registry = state.registry.read().expect("registry lock poisoned");
        registry
            .get_table_live(&tenant, "Order")
            .expect("Order transition table")
            .read()
            .expect("table lock poisoned")
            .clone()
    };
    let (journal, backend) = state.event_journal().expect("sim event journal");
    let recovered = recover_entity_state_from_store(
        tenant.as_str(),
        "Order",
        entity_id,
        &table,
        &journal,
        backend,
        &serde_json::json!({}),
        None,
        true,
    )
    .await
    .expect("strict terminal recovery");
    assert_eq!(recovered.status, "Deleted");
    assert_eq!(recovered.sequence_nr, 3);
}
