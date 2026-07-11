use temper_runtime::ActorSystem;
use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::StorageStack;
use temper_server::registry::SpecRegistry;
use temper_server::storage::QueryPlaneStore;
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;

const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

fn event(event_type: &str, payload: serde_json::Value) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: event_type.to_string(),
        payload,
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: "query-projection-replay-safety".to_string(),
        },
    }
}

fn entity_event(
    event_type: &str,
    action: &str,
    from_status: &str,
    to_status: &str,
    params: serde_json::Value,
) -> PersistenceEnvelope {
    event(
        event_type,
        serde_json::json!({
            "action": action,
            "from_status": from_status,
            "to_status": to_status,
            "timestamp": sim_now(),
            "params": params,
            "idempotency_key": null
        }),
    )
}

async fn state_and_store(name: &str) -> (ServerState, TursoEventStore, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!("temper-{name}-{}.db", sim_uuid()));
    let _ = std::fs::remove_file(&path);
    let store = TursoEventStore::new(&format!("file:{}", path.display()), None)
        .await
        .expect("create local Turso store");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "tenant-a",
        parse_csdl(CSDL_XML).expect("parse fixture CSDL"),
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    (state, store, path)
}

async fn seed_stale_projection(store: &TursoEventStore, entity_id: &str) {
    let fields = serde_json::json!({
        "Id": entity_id,
        "Status": "Draft",
        "Title": "unsafe-stale-projection",
    });
    let state = serde_json::json!({
        "entity_type": "Order",
        "entity_id": entity_id,
        "status": "Draft",
        "fields": fields,
        "sequence_nr": 1,
        "events": [],
    });
    store
        .upsert_projection(
            "tenant-a",
            "Order",
            entity_id,
            "Draft",
            state.get("fields").unwrap(),
            &state,
            1,
        )
        .await
        .expect("seed stale projection");
}

async fn projected_title_ids(store: &TursoEventStore, title: &str) -> Vec<String> {
    store
        .query_field_index(
            "tenant-a",
            "Order",
            "field_name = ?3 AND field_value = ?4",
            vec!["Title".to_string(), title.to_string()],
        )
        .await
        .expect("query title projection")
}

#[tokio::test]
async fn strict_backfill_quarantines_schema_generation_and_tombstone_corruption() {
    let (state, store, path) = state_and_store("strict-replay-quarantine").await;
    let tenant = TenantId::new("tenant-a");

    let malformed_id = "ord-malformed-payload";
    store
        .append(
            &format!("tenant-a:Order:{malformed_id}"),
            0,
            &[event(
                "EntityEvent",
                serde_json::json!({"action": "CancelOrder"}),
            )],
        )
        .await
        .unwrap();
    seed_stale_projection(&store, malformed_id).await;

    let invalid_generation_id = "ord-invalid-generation";
    store
        .append(
            &format!("tenant-a:Order:{invalid_generation_id}"),
            0,
            &[
                entity_event(
                    "Created",
                    "Created",
                    "",
                    "Draft",
                    serde_json::json!({"Title": "old"}),
                ),
                entity_event(
                    "Deleted",
                    "Deleted",
                    "Draft",
                    "Deleted",
                    serde_json::json!({}),
                ),
                entity_event(
                    "EntityEvent",
                    "CancelOrder",
                    "Deleted",
                    "Cancelled",
                    serde_json::json!({"Title": "must-not-revive"}),
                ),
            ],
        )
        .await
        .unwrap();
    seed_stale_projection(&store, invalid_generation_id).await;

    let contradictory_id = "ord-contradictory-tombstone";
    store
        .append(
            &format!("tenant-a:Order:{contradictory_id}"),
            0,
            &[
                entity_event(
                    "Created",
                    "Created",
                    "",
                    "Draft",
                    serde_json::json!({"Title": "old"}),
                ),
                entity_event(
                    "Deleted",
                    "CancelOrder",
                    "Draft",
                    "Cancelled",
                    serde_json::json!({}),
                ),
            ],
        )
        .await
        .unwrap();
    seed_stale_projection(&store, contradictory_id).await;

    let malformed_recreation_id = "ord-malformed-recreation";
    store
        .append(
            &format!("tenant-a:Order:{malformed_recreation_id}"),
            0,
            &[
                entity_event(
                    "Created",
                    "Created",
                    "",
                    "Draft",
                    serde_json::json!({"Title": "old"}),
                ),
                entity_event(
                    "Deleted",
                    "Deleted",
                    "Draft",
                    "Deleted",
                    serde_json::json!({}),
                ),
                entity_event(
                    "Created",
                    "Created",
                    "Deleted",
                    "Cancelled",
                    serde_json::json!({"Title": "impossible recreation"}),
                ),
            ],
        )
        .await
        .unwrap();
    seed_stale_projection(&store, malformed_recreation_id).await;

    let backfill = state.populate_field_index_from_snapshots(&tenant).await;
    assert!(
        backfill.is_err(),
        "corrupt histories must fail the backfill"
    );

    assert!(
        projected_title_ids(&store, "unsafe-stale-projection")
            .await
            .is_empty(),
        "strict recovery errors must quarantine every pre-existing projection"
    );
    assert!(
        state
            .get_tenant_entity_state(&tenant, "Order", invalid_generation_id)
            .await
            .is_err(),
        "actor hydration must reject a non-Created event after deletion"
    );
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn composite_audit_tail_does_not_resurrect_deleted_generation() {
    let (state, store, path) = state_and_store("deleted-composite-audit-tail").await;
    let tenant = TenantId::new("tenant-a");
    let entity_id = "ord-deleted-audit-tail";
    store
        .append(
            &format!("tenant-a:Order:{entity_id}"),
            0,
            &[
                entity_event(
                    "Created",
                    "Created",
                    "",
                    "Draft",
                    serde_json::json!({"Title": "deleted"}),
                ),
                entity_event(
                    "Deleted",
                    "Deleted",
                    "Draft",
                    "Deleted",
                    serde_json::json!({}),
                ),
                event(
                    "CompositeEvent",
                    serde_json::json!({
                        "tenant": "tenant-a",
                        "parent_entity_type": "Order",
                        "parent_entity_id": entity_id,
                        "parent_action": "AfterDeleteAudit",
                        "composite_idempotency_key": "audit-after-delete",
                        "sub_writes": []
                    }),
                ),
            ],
        )
        .await
        .unwrap();
    seed_stale_projection(&store, entity_id).await;

    state
        .populate_field_index_from_snapshots(&tenant)
        .await
        .expect("audit-only tail is a valid deleted history");
    assert!(
        state
            .list_entity_ids_lazy(&tenant, "Order")
            .await
            .unwrap()
            .is_empty(),
        "lifecycle-neutral audit event must not classify the stream live"
    );
    assert!(
        projected_title_ids(&store, "unsafe-stale-projection")
            .await
            .is_empty()
    );
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn both_tombstone_encodings_allow_only_created_recreation() {
    let (state, store, path) = state_and_store("tombstone-recreation").await;
    let tenant = TenantId::new("tenant-a");
    let streams = [
        (
            "ord-event-type-tombstone",
            event("Deleted", serde_json::json!({})),
            "Recreated From Event Type",
        ),
        (
            "ord-payload-action-tombstone",
            entity_event(
                "EntityEvent",
                "Deleted",
                "Draft",
                "Deleted",
                serde_json::json!({}),
            ),
            "Recreated From Payload Action",
        ),
    ];

    for (entity_id, tombstone, new_title) in streams {
        store
            .append(
                &format!("tenant-a:Order:{entity_id}"),
                0,
                &[
                    entity_event(
                        "Created",
                        "Created",
                        "",
                        "Draft",
                        serde_json::json!({"Title": "Old", "Owner": "old-owner"}),
                    ),
                    tombstone,
                    entity_event(
                        "Created",
                        "Created",
                        "",
                        "Draft",
                        serde_json::json!({"Title": new_title}),
                    ),
                ],
            )
            .await
            .unwrap();
    }

    state
        .populate_field_index_from_snapshots(&tenant)
        .await
        .expect("backfill valid replay histories");

    assert_eq!(
        projected_title_ids(&store, "Recreated From Event Type").await,
        vec!["ord-event-type-tombstone".to_string()]
    );
    assert_eq!(
        projected_title_ids(&store, "Recreated From Payload Action").await,
        vec!["ord-payload-action-tombstone".to_string()]
    );
    assert!(projected_title_ids(&store, "Old").await.is_empty());
    let owner_rows = store
        .query_field_index(
            "tenant-a",
            "Order",
            "field_name = ?3 AND field_value = ?4",
            vec!["Owner".to_string(), "old-owner".to_string()],
        )
        .await
        .unwrap();
    assert!(owner_rows.is_empty(), "old-generation fields must be reset");
    let _ = std::fs::remove_file(path);
}
