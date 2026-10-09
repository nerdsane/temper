//! Required post-commit outcomes cannot be certified by a core-state digest.
use super::tests::fixture::*;
use crate::storage::{BackendLabel, BoxedEventStore, QueryPlaneStore, QueryProjectionFieldsRow};
use crate::{ServerState, StorageStack, request_context::AgentContext};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use temper_runtime::{persistence::PersistenceError, tenant::TenantId};
use temper_store_sim::SimEventStore;

struct RejectProjection(AtomicUsize);
#[async_trait]
impl QueryPlaneStore for RejectProjection {
    async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Option<Vec<(String, u64)>>, PersistenceError> {
        Ok(None)
    }
    async fn upsert_projection(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
        _: &Value,
        _: &Value,
        _: u64,
    ) -> Result<(), PersistenceError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(PersistenceError::Storage(
            "required projection rejected".into(),
        ))
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
fn state(store: &SimEventStore, dependent: bool) -> ServerState {
    let source = reaction_spec("Source", if dependent { &["Child"] } else { &[] });
    let child = reaction_spec("Child", &[]);
    let mut state = state_with_specs(&[("Source", &source), ("Child", &child)]);
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    state
}
async fn cold_refuses(store: &SimEventStore) {
    let before = serde_json::to_value(store.dump_journal("default:Source:one")).unwrap();
    let cold = state(store, false); // Removing the required work cannot certify old outcomes.
    for _ in 0..2 {
        let error = cold
            .dispatch_tenant_action(
                &TenantId::default(),
                "Source",
                "one",
                "Start",
                json!({}),
                &AgentContext {
                    idempotency_key: Some("K".into()),
                    ..AgentContext::system()
                },
            )
            .await
            .expect_err("cold retry fabricated a success");
        assert!(error.contains("IdempotencyKeyUnverifiable"), "{error}");
    }
    assert_eq!(
        cold.get_tenant_entity_state(&TenantId::default(), "Source", "one")
            .await
            .unwrap()
            .state
            .status,
        "Running"
    );
    assert_eq!(
        serde_json::to_value(store.dump_journal("default:Source:one")).unwrap(),
        before
    );
}
#[tokio::test]
async fn background_mode_projection_failure_cannot_become_cold_success() {
    let (_guard, _, _) = temper_runtime::scheduler::install_deterministic_context(52351);
    let store = SimEventStore::no_faults(52320);
    let mut state = state(&store, true);
    let projection = Arc::new(RejectProjection(AtomicUsize::new(0)));
    state.storage_stack = Some(Arc::new(StorageStack::new(
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
    let response = run(&state, "Source", false, true).await;
    assert!(!response.success);
    assert!(response.error.unwrap().contains("query projection failed"));
    assert_eq!(projection.0.load(Ordering::SeqCst), 1);
    assert_eq!(action_count(&store, "Source", "Start"), 1);
    assert_eq!(action_count(&store, "Child", "Start"), 0);
    drop(state);
    cold_refuses(&store).await;
    assert_eq!(projection.0.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn awaited_reaction_unknown_cannot_become_cold_success() {
    let (_guard, _, _) = temper_runtime::scheduler::install_deterministic_context(52352);
    let store = SimEventStore::no_faults(52321);
    let state = state(&store, true);
    *state
        .idempotency_cache
        .interrupt_action_once
        .lock()
        .unwrap() = Some(("default".into(), "Child".into(), "Start".into()));
    let response = run(&state, "Source", false, true).await;
    assert!(!response.success);
    assert!(response.error.unwrap().contains("completion is unknown"));
    assert_eq!(action_count(&store, "Child", "Start"), 1);
    drop(state);
    cold_refuses(&store).await;
    assert_eq!(action_count(&store, "Child", "Start"), 1);
}
#[tokio::test]
async fn pure_dispatch_capacity_failure_cannot_become_cold_success() {
    let (_guard, _, _) = temper_runtime::scheduler::install_deterministic_context(52353);
    let store = SimEventStore::no_faults(52322);
    let state = state(&store, false);
    let _pressure = state
        .idempotency_cache
        .continuation_slots
        .clone()
        .try_acquire_many_owned(64)
        .unwrap();
    let response = run(&state, "Source", false, true).await;
    assert!(!response.success);
    assert!(
        response
            .error
            .unwrap()
            .contains("completion capacity exhausted")
    );
    assert_eq!(action_count(&store, "Source", "Start"), 1);
    drop(state);
    cold_refuses(&store).await;
}
