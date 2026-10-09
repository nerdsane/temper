//! Atomic child commitment is not evidence of post-append projection success.
use super::*;
use crate::storage::{BackendLabel, BoxedEventStore, QueryPlaneStore, QueryProjectionFieldsRow};
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use temper_runtime::persistence::PersistenceError;

struct RejectChildProjection(AtomicUsize);
#[async_trait]
impl QueryPlaneStore for RejectChildProjection {
    async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Option<Vec<(String, u64)>>, PersistenceError> {
        Ok(None)
    }
    async fn upsert_projection(
        &self,
        _: &str,
        kind: &str,
        _: &str,
        status: &str,
        _: &Value,
        _: &Value,
        _: u64,
    ) -> Result<(), PersistenceError> {
        if kind == "Child" && status == "Active" {
            self.0.fetch_add(1, Ordering::SeqCst);
            return Err(PersistenceError::Storage(
                "required child projection failed".into(),
            ));
        }
        Ok(())
    }
    async fn remove_projection(&self, _: &str, _: &str, _: &str) -> Result<(), PersistenceError> {
        Ok(())
    }
    async fn query_field_index(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: Vec<String>,
    ) -> Result<Option<Vec<String>>, PersistenceError> {
        Ok(None)
    }
    async fn load_projection_fields_many(
        &self,
        _: &str,
        _: &str,
        _: &[String],
        _: &[&str],
    ) -> Result<Option<Vec<QueryProjectionFieldsRow>>, PersistenceError> {
        Ok(None)
    }
}
#[tokio::test]
async fn atomic_child_post_append_projection_failure_cannot_become_cold_success() {
    let (_guard, _, _) = temper_runtime::scheduler::install_deterministic_context(52360);
    let store = SimEventStore::no_faults(52360);
    let mut warm = composite_test_state_with_store(store.clone());
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");
    warm.get_tenant_entity_state(&tenant, "Parent", "one")
        .await
        .unwrap();
    let before = store.total_events();
    assert_eq!(before, 1);
    let projection = Arc::new(RejectChildProjection(AtomicUsize::new(0)));
    warm.storage_stack = Some(Arc::new(StorageStack::new(
        BackendLabel::Sim,
        BoxedEventStore::new(store.clone()),
        None,
        None,
        None,
        None,
        Some(projection.clone()),
        None,
        None,
        None,
    )));
    let writes = json!({"sub_writes":[{"entity_type":"Child","entity_id":"child","action":"Create","params":{"Name":"once"}}]});
    let error = warm
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "one",
            "CreateChild",
            &writes,
            &agent,
        )
        .await
        .expect_err("injected post-append projection must fail");
    assert!(
        error
            .to_string()
            .contains("required child projection failed"),
        "{error}"
    );
    assert_eq!(projection.0.load(Ordering::SeqCst), 1);
    // Child bootstrap + Create, and the parent's CompositeEvent all committed.
    assert_eq!(store.total_events(), before + 3);
    let child = store.dump_journal("default:Child:child");
    assert_eq!(child.len(), 2);
    let committed = &child
        .iter()
        .find(|e| e.payload["action"] == "Create")
        .unwrap()
        .payload;
    assert_eq!(committed["idempotency_reply"]["mode"]["kind"], "composite");
    let key = committed["idempotency_key"].as_str().unwrap().to_string();
    let parent = serde_json::to_value(store.dump_journal("default:Parent:one")).unwrap();
    drop(warm);
    let cold = composite_test_state_with_store(store.clone());
    for _ in 0..2 {
        let error = cold
            .dispatch_tenant_action(
                &tenant,
                "Child",
                "child",
                "Create",
                json!({"Name":"once"}),
                &AgentContext {
                    idempotency_key: Some(key.clone()),
                    ..AgentContext::for_service("composite-test")
                },
            )
            .await
            .expect_err("core commit cannot certify failed projection");
        assert!(error.contains("IdempotencyKeyUnverifiable"));
    }
    assert_eq!(
        cold.get_tenant_entity_state(&tenant, "Child", "child")
            .await
            .unwrap()
            .state
            .status,
        "Active"
    );
    assert_eq!(store.total_events(), before + 3);
    assert_eq!(
        serde_json::to_value(store.dump_journal("default:Child:child")).unwrap(),
        serde_json::to_value(child).unwrap()
    );
    assert_eq!(
        serde_json::to_value(store.dump_journal("default:Parent:one")).unwrap(),
        parent
    );
    assert_eq!(projection.0.load(Ordering::SeqCst), 1);
    assert!(
        cold.entity_observe_log
            .lock()
            .unwrap()
            .values()
            .flatten()
            .all(|e| e.event_name != "integration_complete")
    );
}
