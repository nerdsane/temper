use super::super::types::QueryPlaneReadResult;
use super::*;
use temper_runtime::persistence::{EntityKeyRow, EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_store_sim::SimEventStore;

fn entity_event(
    event_type: &str,
    action: &str,
    from_status: &str,
    to_status: &str,
    params: serde_json::Value,
) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: event_type.to_string(),
        payload: serde_json::json!({
            "action": action,
            "from_status": from_status,
            "to_status": to_status,
            "timestamp": sim_now(),
            "params": params,
            "idempotency_key": null
        }),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: "journal-validation-test".to_string(),
        },
    }
}

fn eq(property: &str, value: &str) -> FilterExpr {
    FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property(property.to_string())),
        op: BinaryOperator::Eq,
        right: Box::new(FilterExpr::Literal(ODataValue::String(value.to_string()))),
    }
}

async fn local_turso(name: &str) -> (ServerState, TursoEventStore, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!("temper-{name}-{}.db", sim_uuid()));
    let _ = std::fs::remove_file(&path);
    let store = TursoEventStore::new(&format!("file:{}", path.display()), None)
        .await
        .expect("create local Turso store");
    let mut state = build_order_state(name);
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    (state, store, path)
}

async fn project(
    store: &TursoEventStore,
    entity_id: &str,
    title: &str,
    status: &str,
    sequence_nr: u64,
) {
    let fields = serde_json::json!({
        "Id": entity_id,
        "Status": status,
        "Title": title,
    });
    let state = serde_json::json!({
        "entity_type": "Order",
        "entity_id": entity_id,
        "status": status,
        "fields": fields,
        "sequence_nr": sequence_nr,
        "events": [],
    });
    store
        .upsert_query_projection_with_state(
            TenantId::default().as_str(),
            "Order",
            entity_id,
            status,
            state.get("fields").unwrap(),
            &state,
            sequence_nr,
        )
        .await
        .expect("upsert projection");
}

async fn read_title(
    state: &ServerState,
    title: &str,
    top: usize,
    default_page_size: usize,
    max_entities: usize,
) -> Result<QueryPlaneReadResult, QueryPlaneReadError> {
    let tenant = TenantId::default();
    let security_ctx = SecurityContext::system();
    let query_options = QueryOptions {
        filter: Some(eq("Title", title)),
        orderby: Some(vec![OrderByClause {
            property: "Id".to_string(),
            direction: OrderDirection::Asc,
        }]),
        top: Some(top),
        ..QueryOptions::default()
    };
    read_entity_set_page(QueryPlaneReadRequest {
        state,
        tenant: &tenant,
        security_ctx: &security_ctx,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &query_options,
        budget: QueryPlaneReadBudget {
            default_page_size,
            max_entities,
        },
    })
    .await
}

fn expect_read_ok(
    result: Result<QueryPlaneReadResult, QueryPlaneReadError>,
) -> QueryPlaneReadResult {
    match result {
        Ok(result) => result,
        Err(_) => panic!("expected OData read to succeed"),
    }
}

#[tokio::test]
async fn projection_orphan_and_tombstone_are_not_readable() {
    let (state, store, path) = local_turso("projection-liveness").await;
    let tenant = TenantId::default();
    project(&store, "a-orphan", "stale", "Draft", 1).await;
    store
        .append(
            "default:Order:b-deleted",
            0,
            &[
                entity_event(
                    "Created",
                    "Created",
                    "",
                    "Draft",
                    serde_json::json!({"Title": "stale"}),
                ),
                entity_event(
                    "Deleted",
                    "Deleted",
                    "Draft",
                    "Deleted",
                    serde_json::json!({}),
                ),
            ],
        )
        .await
        .unwrap();
    project(&store, "b-deleted", "stale", "Draft", 1).await;

    let result = expect_read_ok(read_title(&state, "stale", 10, 2, 10).await);
    assert!(result.entities.is_empty());
    assert!(state.list_entity_ids(&tenant, "Order").is_empty());
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn stale_projection_rows_do_not_skip_live_rows_across_native_pages() {
    let (state, store, path) = local_turso("projection-pagination").await;
    for id in ["a-stale", "b-live", "c-live"] {
        project(&store, id, "match", "Draft", 1).await;
    }
    for id in ["b-live", "c-live"] {
        store
            .append(
                &format!("default:Order:{id}"),
                0,
                &[entity_event(
                    "Created",
                    "Created",
                    "",
                    "Draft",
                    serde_json::json!({"Title": "match"}),
                )],
            )
            .await
            .unwrap();
    }

    let result = expect_read_ok(read_title(&state, "match", 2, 1, 2).await);
    let ids = result
        .entities
        .iter()
        .filter_map(|entity| entity["entity_id"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["b-live", "c-live"]);
    assert_eq!(result.telemetry.candidate_count, 3);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn stale_only_native_pages_consume_the_candidate_budget() {
    let (state, store, path) = local_turso("projection-stale-budget").await;
    for index in 0..11 {
        project(
            &store,
            &format!("orphan-{index:02}"),
            "stale-budget",
            "Draft",
            1,
        )
        .await;
    }

    let error = match read_title(&state, "stale-budget", 1, 1, 1).await {
        Ok(_) => panic!("stale projections must not drive an unbounded scan"),
        Err(error) => error,
    };
    match error {
        QueryPlaneReadError::QueryTooLarge { telemetry } => {
            assert_eq!(telemetry.candidate_count, 10);
        }
        _ => panic!("expected QueryTooLarge"),
    }
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn stale_catalog_and_running_actor_recover_to_proven_journal_sequence() {
    let (state, store, path) = local_turso("projection-sequence").await;
    let tenant = TenantId::default();
    let persistence_id = "default:Order:sequence-target";
    store
        .append(
            persistence_id,
            0,
            &[entity_event(
                "Created",
                "Created",
                "",
                "Draft",
                serde_json::json!({"Title": "old-owner"}),
            )],
        )
        .await
        .unwrap();
    assert!(
        state
            .ensure_entity_loaded(&tenant, "Order", "sequence-target")
            .await
    );
    project(&store, "sequence-target", "old-owner", "Draft", 1).await;

    // Simulate a remote/composite writer advancing the journal while the local
    // actor and catalog remain at sequence 1.
    store
        .append(
            persistence_id,
            1,
            &[entity_event(
                "CancelOrder",
                "CancelOrder",
                "Draft",
                "Cancelled",
                serde_json::json!({"Title": "new-owner"}),
            )],
        )
        .await
        .unwrap();

    let stale = expect_read_ok(read_title(&state, "old-owner", 1, 1, 10).await);
    assert!(
        stale.entities.is_empty(),
        "stale projection attributes must never reach filter/auth evaluation"
    );
    let current = state
        .get_tenant_entity_state(&tenant, "Order", "sequence-target")
        .await
        .unwrap();
    assert_eq!(current.state.sequence_nr, 2);
    assert_eq!(current.state.status, "Cancelled");
    assert_eq!(current.state.fields["Title"], "new-owner");
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn deleted_declared_key_is_released_and_exact_lookup_is_empty() {
    let tenant = TenantId::default();
    let mut state = build_order_state("deleted-keyed-read");
    let store = SimEventStore::no_faults(192_003);
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    let fields = serde_json::json!({
        "WorkspaceId": "ws-deleted",
        "Path": "/deleted.txt",
    });
    let key_hash = crate::key_index::canonical_key_hash(
        "ws_path",
        &["WorkspaceId".to_string(), "Path".to_string()],
        fields.as_object().unwrap(),
    )
    .unwrap();
    let key = EntityKeyRow {
        key_name: "ws_path".to_string(),
        key_hash: key_hash.clone(),
    };
    store
        .append_with_keys(
            "default:Order:keyed-deleted",
            0,
            &[entity_event("Created", "Created", "", "Draft", fields)],
            std::slice::from_ref(&key),
        )
        .await
        .unwrap();
    store
        .mark_key_index_backfilled("default", "Order", "ws_path")
        .await
        .unwrap();
    store
        .append_with_keys(
            "default:Order:keyed-deleted",
            1,
            &[entity_event(
                "Deleted",
                "Deleted",
                "Draft",
                "Deleted",
                serde_json::json!({}),
            )],
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .lookup_by_key("default", "Order", "ws_path", &key_hash)
            .await
            .unwrap(),
        None
    );

    let security_ctx = SecurityContext::system();
    let query_options = QueryOptions {
        filter: Some(FilterExpr::BinaryOp {
            left: Box::new(eq("WorkspaceId", "ws-deleted")),
            op: BinaryOperator::And,
            right: Box::new(eq("Path", "/deleted.txt")),
        }),
        top: Some(1),
        ..QueryOptions::default()
    };
    let result = expect_read_ok(
        read_entity_set_page(QueryPlaneReadRequest {
            state: &state,
            tenant: &tenant,
            security_ctx: &security_ctx,
            entity_type: "Order",
            entity_set_name: "Orders",
            query_options: &query_options,
            budget: QueryPlaneReadBudget {
                default_page_size: 1,
                max_entities: 1,
            },
        })
        .await,
    );
    assert!(result.entities.is_empty());
}
