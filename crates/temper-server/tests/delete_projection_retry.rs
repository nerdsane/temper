use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_runtime::ActorSystem;
use temper_runtime::persistence::{EventStore, PersistenceError};
use temper_runtime::tenant::TenantId;
use temper_server::storage::{QueryPlaneStore, QueryProjectionFieldsRow, StorageStack};
use temper_server::{ServerState, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

struct FailOnceRemoveProjection {
    inner: TursoEventStore,
    fail_next_remove: AtomicBool,
    fail_next_upsert: AtomicBool,
    upsert_attempts: AtomicUsize,
}

#[async_trait::async_trait]
impl QueryPlaneStore for FailOnceRemoveProjection {
    async fn upsert_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        state: &serde_json::Value,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        self.upsert_attempts.fetch_add(1, Ordering::SeqCst);
        if self.fail_next_upsert.swap(false, Ordering::SeqCst) {
            return Err(PersistenceError::Storage(
                "injected projection upsert failure".to_string(),
            ));
        }
        QueryPlaneStore::upsert_projection(
            &self.inner,
            tenant,
            entity_type,
            entity_id,
            status,
            fields,
            state,
            sequence_nr,
        )
        .await
    }

    async fn remove_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), PersistenceError> {
        QueryPlaneStore::remove_projection(&self.inner, tenant, entity_type, entity_id).await
    }

    async fn remove_projection_through_sequence(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        if self.fail_next_remove.swap(false, Ordering::SeqCst) {
            return Err(PersistenceError::Storage(
                "injected projection removal failure".to_string(),
            ));
        }
        QueryPlaneStore::remove_projection_through_sequence(
            &self.inner,
            tenant,
            entity_type,
            entity_id,
            sequence_nr,
        )
        .await
    }

    async fn query_field_index(
        &self,
        tenant: &str,
        entity_type: &str,
        where_clause: &str,
        params: Vec<String>,
    ) -> Result<Option<Vec<String>>, PersistenceError> {
        QueryPlaneStore::query_field_index(&self.inner, tenant, entity_type, where_clause, params)
            .await
    }

    async fn load_projection_fields_many(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
        field_names: &[&str],
    ) -> Result<Option<Vec<QueryProjectionFieldsRow>>, PersistenceError> {
        QueryPlaneStore::load_projection_fields_many(
            &self.inner,
            tenant,
            entity_type,
            entity_ids,
            field_names,
        )
        .await
    }

    async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Option<Vec<(String, u64)>>, PersistenceError> {
        QueryPlaneStore::projected_entity_counts_by_tenant(&self.inner).await
    }
}

#[tokio::test]
async fn projection_removal_failure_is_retryable_without_second_tombstone() {
    let path = std::env::temp_dir().join(format!(
        "temper-delete-projection-retry-{}.db",
        uuid::Uuid::new_v4()
    ));
    let _ = std::fs::remove_file(&path);
    let store = TursoEventStore::new(&format!("file:{}", path.display()), None)
        .await
        .expect("create local Turso store");
    let mut specs = std::collections::BTreeMap::new();
    specs.insert("Order".to_string(), ORDER_IOA.to_string());
    let mut state = ServerState::with_specs(
        ActorSystem::new("delete-projection-retry"),
        parse_csdl(CSDL_XML).expect("parse fixture CSDL"),
        CSDL_XML.to_string(),
        specs,
    )
    .expect("build server state");
    let mut storage = StorageStack::from_turso(store.clone());
    storage.query_plane = Some(Arc::new(FailOnceRemoveProjection {
        inner: store.clone(),
        fail_next_remove: AtomicBool::new(true),
        fail_next_upsert: AtomicBool::new(false),
        upsert_attempts: AtomicUsize::new(0),
    }));
    state.set_storage_stack(storage);

    let tenant = TenantId::default();
    let entity_id = "ord-delete-retry";
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    state
        .get_or_create_tenant_entity(
            &tenant,
            "Order",
            entity_id,
            serde_json::json!({"Title": "Delete Retry"}),
        )
        .await
        .expect("create entity");

    let router = build_router(state.clone());
    let first = router
        .clone()
        .oneshot(
            Request::delete("/tdata/Orders('ord-delete-retry')")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let first = first.expect("first delete response");
    assert_eq!(first.status(), StatusCode::SERVICE_UNAVAILABLE);
    let first_body = axum::body::to_bytes(first.into_body(), 1_000_000)
        .await
        .expect("read first delete response body");
    let first_json: serde_json::Value =
        serde_json::from_slice(&first_body).expect("parse first delete response body");
    assert_eq!(first_json["error"]["code"], "DeleteUnavailable");
    assert!(
        !first_json
            .to_string()
            .contains("injected projection removal failure"),
        "storage diagnostics must not be exposed to clients"
    );
    let after_first = store.read_events(&persistence_id, 0).await.unwrap();
    assert_eq!(after_first.len(), 2);
    assert_eq!(after_first.last().unwrap().event_type, "Deleted");
    assert!(!state.entity_exists(&tenant, "Order", entity_id));
    assert_eq!(
        store
            .query_field_index(
                tenant.as_str(),
                "Order",
                "field_name = ?3 AND field_value = ?4",
                vec!["Title".to_string(), "Delete Retry".to_string()],
            )
            .await
            .unwrap(),
        vec![entity_id.to_string()],
        "first failure intentionally leaves the stale projection for retry"
    );

    state
        .authz
        .reload_tenant_policies(
            tenant.as_str(),
            r#"forbid(principal, action == Action::"delete", resource);"#,
        )
        .expect("install status-independent delete denial after tombstone");

    let retried = router
        .oneshot(
            Request::delete("/tdata/Orders('ord-delete-retry')")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("retry delete response");
    assert_eq!(retried.status(), StatusCode::NO_CONTENT);
    let after_retry = store.read_events(&persistence_id, 0).await.unwrap();
    assert_eq!(
        after_retry.len(),
        after_first.len(),
        "retry must not append a second tombstone"
    );
    assert!(
        store
            .query_field_index(
                tenant.as_str(),
                "Order",
                "field_name = ?3 AND field_value = ?4",
                vec!["Title".to_string(), "Delete Retry".to_string()],
            )
            .await
            .unwrap()
            .is_empty()
    );
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn field_projection_retry_converges_without_duplicate_journal_event() {
    let path = std::env::temp_dir().join(format!(
        "temper-field-projection-retry-{}.db",
        uuid::Uuid::new_v4()
    ));
    let _ = std::fs::remove_file(&path);
    let store = TursoEventStore::new(&format!("file:{}", path.display()), None)
        .await
        .expect("create local Turso store");
    let mut specs = std::collections::BTreeMap::new();
    specs.insert("Order".to_string(), ORDER_IOA.to_string());
    let mut state = ServerState::with_specs(
        ActorSystem::new("field-projection-retry"),
        parse_csdl(CSDL_XML).expect("parse fixture CSDL"),
        CSDL_XML.to_string(),
        specs,
    )
    .expect("build server state");
    let projection = Arc::new(FailOnceRemoveProjection {
        inner: store.clone(),
        fail_next_remove: AtomicBool::new(false),
        fail_next_upsert: AtomicBool::new(false),
        upsert_attempts: AtomicUsize::new(0),
    });
    let mut storage = StorageStack::from_turso(store.clone());
    storage.query_plane = Some(projection.clone());
    state.set_storage_stack(storage);

    let tenant = TenantId::default();
    let entity_id = "ord-field-projection-retry";
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    state
        .get_or_create_tenant_entity(
            &tenant,
            "Order",
            entity_id,
            serde_json::json!({"Title": "Before"}),
        )
        .await
        .expect("create entity and initial projection");
    projection.fail_next_upsert.store(true, Ordering::SeqCst);
    let attempts_before = projection.upsert_attempts.load(Ordering::SeqCst);

    let response = build_router(state.clone())
        .oneshot(
            Request::patch("/tdata/Orders('ord-field-projection-retry')")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"Title":"After"}"#))
                .unwrap(),
        )
        .await
        .expect("field update response");
    assert_eq!(response.status(), StatusCode::OK);

    let mut converged = false;
    for _ in 0..100 {
        let ids = store
            .query_field_index(
                tenant.as_str(),
                "Order",
                "field_name = ?3 AND field_value = ?4",
                vec!["Title".to_string(), "After".to_string()],
            )
            .await
            .expect("query projection while waiting");
        if ids == vec![entity_id.to_string()] {
            converged = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        converged,
        "projection retry queue must converge the derived row"
    );
    assert!(
        projection.upsert_attempts.load(Ordering::SeqCst) >= attempts_before + 2,
        "one failed worker attempt and one successful retry must be observable"
    );

    let journal = store
        .read_events(&persistence_id, 0)
        .await
        .expect("read field-update journal");
    assert_eq!(
        journal
            .iter()
            .filter(|event| event.event_type == "FieldsPatched")
            .count(),
        1,
        "projection retry must never append the source event again"
    );

    let mut restart_specs = std::collections::BTreeMap::new();
    restart_specs.insert("Order".to_string(), ORDER_IOA.to_string());
    let mut restarted = ServerState::with_specs(
        ActorSystem::new("field-projection-retry-restart"),
        parse_csdl(CSDL_XML).expect("parse restart CSDL"),
        CSDL_XML.to_string(),
        restart_specs,
    )
    .expect("build restarted server state");
    restarted.set_storage_stack(StorageStack::from_turso(store.clone()));
    let replayed = restarted
        .get_tenant_entity_state(&tenant, "Order", entity_id)
        .await
        .expect("replay field update after restart");
    assert_eq!(replayed.state.fields["Title"], "After");
    let restarted_journal = store.read_events(&persistence_id, 0).await.unwrap();
    assert_eq!(
        restarted_journal
            .iter()
            .filter(|event| event.event_type == "FieldsPatched")
            .count(),
        1
    );
    let _ = std::fs::remove_file(path);
}
