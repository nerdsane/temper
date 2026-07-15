//! Backend-capability regressions for the declared-key absence oracle.

use super::*;
use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::{sim_now, sim_uuid};

/// Turso can probe legacy key rows but does not co-commit or repair them. A no-op
/// backfill must therefore never make a keyed miss authoritative in the server's
/// in-memory watermark cache, even when the type currently has no entities.
#[tokio::test]
async fn non_authoritative_turso_backfill_never_marks_key_index_complete() {
    let store = TursoEventStore::new("file::memory:", None)
        .await
        .expect("create in-memory Turso store");
    let mut state = build_order_state("turso-key-authority");
    state.set_storage_stack(StorageStack::from_turso(store));
    let tenant = TenantId::default();

    state.populate_key_index_from_snapshots(&tenant).await;

    assert!(
        !state
            .key_index_backfill_complete(&tenant, "Order", "v2|ws_path")
            .await,
        "a backend without co-committed exact key reconciliation must remain scan-safe"
    );
}

fn exact_key_filter(workspace: &str, path: &str) -> FilterExpr {
    FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("WorkspaceId".to_string())),
            op: BinaryOperator::Eq,
            right: Box::new(FilterExpr::Literal(ODataValue::String(
                workspace.to_string(),
            ))),
        }),
        op: BinaryOperator::And,
        right: Box::new(FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("Path".to_string())),
            op: BinaryOperator::Eq,
            right: Box::new(FilterExpr::Literal(ODataValue::String(path.to_string()))),
        }),
    }
}

/// Recognizing an exact declared key is itself an authority boundary. A backend
/// without co-committed key ownership must scan the journal rather than falling
/// back to a stale native/catalog projection.
#[tokio::test]
async fn non_authoritative_turso_exact_key_read_ignores_stale_live_projection() {
    let db_url = format!("file:/tmp/temper-arn238-turso-authority-{}.db", sim_uuid());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create in-memory Turso store");
    let mut state = build_order_state("turso-exact-key-authority");
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    let tenant = TenantId::default();
    let entity_id = "turso-legacy-deleted";
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    let fields = serde_json::json!({
        "Id": entity_id,
        "Status": "Draft",
        "WorkspaceId": "ws-turso",
        "Path": "/stale"
    });
    let created_at = sim_now();
    let created = crate::entity_actor::EntityEvent {
        action: "Created".to_string(),
        from_status: String::new(),
        to_status: "Draft".to_string(),
        timestamp: created_at,
        params: fields.clone(),
        idempotency_key: None,
    };
    store
        .append(
            &persistence_id,
            0,
            &[PersistenceEnvelope {
                sequence_nr: 1,
                event_type: "Created".to_string(),
                payload: serde_json::to_value(created).expect("serialize create"),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp: created_at,
                    actor_id: persistence_id.clone(),
                },
            }],
        )
        .await
        .expect("seed durable create");
    let stale_state = serde_json::json!({
        "entity_type": "Order",
        "entity_id": entity_id,
        "status": "Draft",
        "fields": fields,
        "sequence_nr": 1,
        "events": [],
    });
    store
        .upsert_query_projection_with_state(
            tenant.as_str(),
            "Order",
            entity_id,
            "Draft",
            &stale_state["fields"],
            &stale_state,
            1,
        )
        .await
        .expect("seed stale live projection");
    let timestamp = sim_now();
    let deleted = crate::entity_actor::EntityEvent {
        action: "Deleted".to_string(),
        from_status: "Draft".to_string(),
        to_status: "Deleted".to_string(),
        timestamp,
        params: serde_json::json!({}),
        idempotency_key: None,
    };
    store
        .append(
            &persistence_id,
            1,
            &[PersistenceEnvelope {
                sequence_nr: 2,
                event_type: "Deleted".to_string(),
                payload: serde_json::to_value(deleted).expect("serialize tombstone"),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp,
                    actor_id: persistence_id.clone(),
                },
            }],
        )
        .await
        .expect("append event-only tombstone");

    let security_ctx = SecurityContext::system();
    let budget = QueryPlaneReadBudget {
        default_page_size: 10,
        max_entities: 10,
    };
    for (count, ordered) in [(false, false), (true, false), (false, true)] {
        let options = QueryOptions {
            filter: Some(exact_key_filter("ws-turso", "/stale")),
            count: count.then_some(true),
            orderby: ordered.then(|| {
                vec![OrderByClause {
                    property: "Path".to_string(),
                    direction: OrderDirection::Asc,
                }]
            }),
            ..QueryOptions::default()
        };
        let result = read_entity_set_page(QueryPlaneReadRequest {
            state: &state,
            tenant: &tenant,
            security_ctx: &security_ctx,
            entity_type: "Order",
            entity_set_name: "Orders",
            query_options: &options,
            budget,
        })
        .await;
        let result = match result {
            Ok(result) => result,
            Err(_) => panic!("exact-key read remains bounded"),
        };
        assert!(
            result.entities.is_empty(),
            "count={count}, ordered={ordered}: durable tombstone must beat stale Turso projection"
        );
        if count {
            assert_eq!(result.count, Some(0));
        }
    }
}
