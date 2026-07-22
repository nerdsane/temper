//! Composite-key URL fallback must remain authoritative before index coverage.

use std::sync::Arc;

use super::*;
use crate::registry::SpecRegistry;
use crate::storage::{
    BackendLabel, BoxedEventStore, EntityCatalogRow, QueryPlaneStore, QueryProjectionFieldsRow,
    StorageStack,
};
use async_trait::async_trait;
use temper_runtime::ActorSystem;
use temper_runtime::persistence::{
    EntityKeyRow, EventMetadata, EventStore, IndexReconciliation, PersistenceAppend,
    PersistenceEnvelope,
};
use temper_runtime::scheduler::{install_deterministic_context, sim_now, sim_uuid};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

const CSDL_XML: &str = include_str!("../../../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../../../test-fixtures/specs/order.ioa.toml");

struct StaleCatalogQueryPlane {
    row: EntityCatalogRow,
}

#[async_trait]
impl QueryPlaneStore for StaleCatalogQueryPlane {
    async fn upsert_projection(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _entity_id: &str,
        _status: &str,
        _fields: &serde_json::Value,
        _state: &serde_json::Value,
        _sequence_nr: u64,
    ) -> Result<(), temper_runtime::persistence::PersistenceError> {
        Ok(())
    }

    async fn remove_projection(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _entity_id: &str,
    ) -> Result<(), temper_runtime::persistence::PersistenceError> {
        Ok(())
    }

    async fn query_field_index(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _where_clause: &str,
        _params: Vec<String>,
    ) -> Result<Option<Vec<String>>, temper_runtime::persistence::PersistenceError> {
        Ok(None)
    }

    async fn load_projection_fields_many(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _entity_ids: &[String],
        _field_names: &[&str],
    ) -> Result<Option<Vec<QueryProjectionFieldsRow>>, temper_runtime::persistence::PersistenceError>
    {
        Ok(None)
    }

    async fn load_entity_catalog_rows(
        &self,
        _tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
    ) -> Result<Option<Vec<EntityCatalogRow>>, temper_runtime::persistence::PersistenceError> {
        let rows = if entity_type == "Order" && entity_ids.contains(&self.row.entity_id) {
            vec![self.row.clone()]
        } else {
            Vec::new()
        };
        Ok(Some(rows))
    }

    async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Option<Vec<(String, u64)>>, temper_runtime::persistence::PersistenceError> {
        Ok(None)
    }
}

/// A pre-coverage entity may have durable key fields but no key row. Composite
/// URL resolution must scan replayable state and preserve the working lookup,
/// never delegate ownership to the asynchronous field projection.
#[tokio::test]
async fn composite_key_without_coverage_resolves_from_authoritative_state() {
    let (_guard, _clock, _ids) = install_deterministic_context(247);
    let tenant = TenantId::default();
    let entity_id = "ord-composite-pre-coverage";
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    let workspace = "ws-composite";
    let path = "/pre-coverage";
    let store = SimEventStore::no_faults(247);
    let timestamp = sim_now();
    EventStore::append(
        &store,
        &persistence_id,
        0,
        &[PersistenceEnvelope {
            sequence_nr: 0,
            event_type: "Create".to_string(),
            payload: serde_json::json!({
                "action": "Create",
                "from_status": "Draft",
                "to_status": "Draft",
                "timestamp": timestamp,
                "params": {"WorkspaceId": workspace, "Path": path},
                "idempotency_key": null,
            }),
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
    .expect("seed legacy stream without a key row");
    EventStore::save_snapshot(
        &store,
        &persistence_id,
        1,
        &serde_json::to_vec(&serde_json::json!({
            "entity_type": "Order",
            "entity_id": entity_id,
            "status": "Draft",
            "item_count": 0,
            "fields": {
                "Id": entity_id,
                "Status": "Draft",
                "WorkspaceId": workspace,
                "Path": path,
            },
        }))
        .expect("snapshot serialization"),
    )
    .await
    .expect("seed replayable key fields");

    let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        tenant.as_str(),
        csdl,
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA)],
    );
    let mut state =
        ServerState::from_registry(ActorSystem::new("composite-key-pre-coverage"), registry);
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));

    assert!(
        EventStore::lookup_by_key(
            &store,
            tenant.as_str(),
            "Order",
            "ws_path",
            &crate::key_index::canonical_key_hash(
                "ws_path",
                &["WorkspaceId".to_string(), "Path".to_string()],
                serde_json::json!({"WorkspaceId": workspace, "Path": path})
                    .as_object()
                    .expect("key fields"),
            )
            .expect("complete key"),
        )
        .await
        .expect("key lookup")
        .is_none(),
        "precondition: the legacy stream has no declared-key row"
    );

    let resolved = try_resolve_composite_entity_key(
        &state,
        &tenant,
        "Order",
        &[
            ("WorkspaceId".to_string(), workspace.to_string()),
            ("Path".to_string(), path.to_string()),
        ],
    )
    .await;
    let resolved = match resolved {
        Ok(resolved) => resolved,
        Err(_) => panic!("pre-coverage composite lookup must remain within budget"),
    };
    assert_eq!(
        resolved
            .as_ref()
            .map(|resolved| resolved.entity_id.as_str()),
        Some(entity_id)
    );
}

/// Even after key coverage is complete, the catalog is not the body authority.
/// A covered hit identifies the entity, then composite-key GET materializes its
/// current journal/snapshot state so a lagging pre-rename catalog row cannot leak.
#[tokio::test]
async fn covered_composite_hit_materializes_authoritative_body_not_stale_catalog() {
    let (_guard, _clock, _ids) = install_deterministic_context(248);
    let tenant = TenantId::default();
    let entity_id = "ord-composite-covered";
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    let workspace = "ws-composite";
    let current_path = "/current";
    let stale_path = "/stale";
    let store = SimEventStore::no_faults(248);
    let timestamp = sim_now();
    let current_fields = serde_json::json!({
        "Id": entity_id,
        "Status": "Draft",
        "WorkspaceId": workspace,
        "Path": current_path,
    });
    let current_state = serde_json::json!({
        "entity_type": "Order",
        "entity_id": entity_id,
        "status": "Draft",
        "item_count": 0,
        "fields": current_fields,
    });
    let signature = crate::key_index::declared_key_set_signature(
        &temper_jit::table::TransitionTable::from_ioa_source(ORDER_IOA).keys,
    );
    EventStore::append_with_index_rows(
        &store,
        &persistence_id,
        0,
        &[PersistenceEnvelope {
            sequence_nr: 0,
            event_type: "Create".to_string(),
            payload: serde_json::json!({
                "action": "Create",
                "from_status": "Draft",
                "to_status": "Draft",
                "timestamp": timestamp,
                "params": {"WorkspaceId": workspace, "Path": current_path},
                "idempotency_key": null,
            }),
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp,
                actor_id: persistence_id.clone(),
            },
        }],
        &[EntityKeyRow {
            key_name: "ws_path".to_string(),
            key_hash: crate::key_index::canonical_key_hash(
                "ws_path",
                &["WorkspaceId".to_string(), "Path".to_string()],
                current_fields.as_object().expect("current fields"),
            )
            .expect("complete current key"),
        }],
        &[],
        IndexReconciliation {
            keys: true,
            key_set_signature: Some(signature.clone()),
            vectors: false,
            snapshot_source: Default::default(),
        },
    )
    .await
    .expect("seed authoritative key owner");
    EventStore::save_snapshot(
        &store,
        &persistence_id,
        1,
        &serde_json::to_vec(&current_state).expect("snapshot serialization"),
    )
    .await
    .expect("seed current authoritative state");
    EventStore::mark_key_index_backfilled(&store, tenant.as_str(), "Order", &signature)
        .await
        .expect("mark current coverage");

    let stale_fields = serde_json::json!({
        "Id": entity_id,
        "Status": "Draft",
        "WorkspaceId": workspace,
        "Path": stale_path,
    });
    let stale_state = serde_json::json!({
        "entity_type": "Order",
        "entity_id": entity_id,
        "status": "Draft",
        "fields": stale_fields,
        "sequence_nr": 0,
        "events": [],
    });
    let query_plane = Arc::new(StaleCatalogQueryPlane {
        row: EntityCatalogRow {
            entity_id: entity_id.to_string(),
            status: "Draft".to_string(),
            fields: stale_state["fields"].clone(),
            state: Some(stale_state),
            sequence_nr: 0,
        },
    });

    let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        tenant.as_str(),
        csdl,
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA)],
    );
    let mut state =
        ServerState::from_registry(ActorSystem::new("composite-key-covered-body"), registry);
    state.set_storage_stack(StorageStack::new(
        BackendLabel::Sim,
        BoxedEventStore::new(store),
        None,
        None,
        None,
        None,
        Some(query_plane as Arc<dyn QueryPlaneStore>),
        None,
        None,
        None,
    ));

    let security_ctx = SecurityContext::system();
    let composite_key = KeyValue::Composite(vec![
        ("WorkspaceId".to_string(), workspace.to_string()),
        ("Path".to_string(), current_path.to_string()),
    ]);
    let resolved = match resolve_entity_request_key(&state, &tenant, "Order", &composite_key).await
    {
        Ok(resolved) => resolved,
        Err(_) => panic!("covered key hit must resolve"),
    };
    assert_eq!(resolved.entity_id, entity_id);
    let query_options = QueryOptions::default();
    let body = build_entity_body(
        &state,
        &tenant,
        EntityBodyRequest {
            entity_type: "Order",
            set_name: "Orders",
            key: &resolved.entity_id,
            security_ctx: &security_ctx,
            authoritative_body: resolved.authoritative_body,
            context: "$metadata#Orders/$entity".to_string(),
            odata_id: None,
            query_options: &query_options,
            enrich: false,
            function: None,
            select_before_expand: false,
        },
    )
    .await;
    let body = match body {
        Ok(body) => body,
        Err(_) => panic!("covered composite hit must load authoritative body"),
    };
    assert_eq!(body["fields"]["Path"], current_path);
    assert_ne!(body["fields"]["Path"], stale_path);

    // A composite-key URL is still an entity GET. Its internal identity lookup
    // must not add the collection-level `list` permission that direct-ID GET does
    // not require; the final body remains protected by entity `read`.
    state
        .authz
        .reload_tenant_policies(
            tenant.as_str(),
            r#"
                permit(principal, action == Action::"read", resource is Order);
                forbid(principal, action == Action::"list", resource is Order);
            "#,
        )
        .expect("install read-without-list policy");
    let reader = SecurityContext::from_headers(&[(
        "x-temper-principal-id".to_string(),
        "composite-reader".to_string(),
    )]);
    assert!(
        state
            .authorize_with_context(
                &reader,
                "list",
                "Order",
                &std::collections::BTreeMap::new(),
                tenant.as_str(),
            )
            .is_err(),
        "precondition: the reader has no collection list permission"
    );
    let response = handle_entity(
        &state,
        &tenant,
        &reader,
        "Orders",
        &composite_key,
        &QueryOptions::default(),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "composite GET must retain entity read-without-list semantics"
    );
}

/// An incomplete key index over the bounded scan budget must preserve the
/// query-plane 413 instead of degrading to a raw composite string and 404.
#[tokio::test]
async fn oversized_incomplete_composite_lookup_returns_query_too_large() {
    let (_guard, _clock, _ids) = install_deterministic_context(249);
    let tenant = TenantId::default();
    let store = SimEventStore::no_faults(249);
    // The paging wrapper gives the one-row lookahead a ten-candidate headroom
    // (`max_entities + 1`, multiplied by the scan factor). Exceed that effective
    // budget so this exercises the HTTP 413 mapping rather than a bounded miss.
    let scan_budget = QueryPlaneReadBudget::from_config()
        .candidate_budget()
        .saturating_add(10);
    let timestamp = sim_now();
    let event = PersistenceEnvelope {
        sequence_nr: 0,
        event_type: "LegacyCreate".to_string(),
        payload: serde_json::json!({}),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp,
            actor_id: "composite-budget-seed".to_string(),
        },
    };
    let appends = (0..=scan_budget)
        .map(|index| PersistenceAppend {
            persistence_id: format!("{tenant}:Order:ord-budget-{index:05}"),
            expected_sequence: 0,
            events: vec![event.clone()],
            key_rows: Vec::new(),
            reconcile_keys: false,
            key_set_signature: None,
            snapshot_source: Default::default(),
            batch_idempotency: None,
        })
        .collect::<Vec<_>>();
    EventStore::append_batch(&store, &appends)
        .await
        .expect("seed over-budget legacy streams");

    let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        tenant.as_str(),
        csdl,
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA)],
    );
    let mut state =
        ServerState::from_registry(ActorSystem::new("composite-key-over-budget"), registry);
    state.set_storage_stack(StorageStack::from_sim(store, None));
    let key = KeyValue::Composite(vec![
        ("WorkspaceId".to_string(), "missing-workspace".to_string()),
        ("Path".to_string(), "/missing".to_string()),
    ]);
    let response = handle_entity(
        &state,
        &tenant,
        &SecurityContext::system(),
        "Orders",
        &key,
        &QueryOptions::default(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}
