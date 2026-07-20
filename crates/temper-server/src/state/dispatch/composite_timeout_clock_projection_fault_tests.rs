use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::json;
use temper_runtime::ActorSystem;
use temper_runtime::persistence::PersistenceError;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

use crate::request_context::AgentContext;
use crate::state::ServerState;
use crate::storage::{QueryPlaneStore, QueryProjectionFieldsRow, StorageStack};

use super::{CSDL, PARENT_IOA, TIMED_CHILD_WITH_RESET_IOA};

struct FailingQueryPlane {
    fail_writes: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl QueryPlaneStore for FailingQueryPlane {
    async fn upsert_projection(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _entity_id: &str,
        _status: &str,
        _fields: &serde_json::Value,
        _state: &serde_json::Value,
        _sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        if self.fail_writes.load(Ordering::SeqCst) {
            Err(PersistenceError::Storage(
                "injected query projection failure".to_string(),
            ))
        } else {
            Ok(())
        }
    }

    async fn remove_projection(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _entity_id: &str,
        _sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        if self.fail_writes.load(Ordering::SeqCst) {
            Err(PersistenceError::Storage(
                "injected query projection removal failure".to_string(),
            ))
        } else {
            Ok(())
        }
    }

    async fn query_field_index(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _where_clause: &str,
        _params: Vec<String>,
    ) -> Result<Option<Vec<String>>, PersistenceError> {
        Ok(None)
    }

    async fn load_projection_fields_many(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _entity_ids: &[String],
        _field_names: &[&str],
    ) -> Result<Option<Vec<QueryProjectionFieldsRow>>, PersistenceError> {
        Ok(None)
    }

    async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Option<Vec<(String, u64)>>, PersistenceError> {
        Ok(None)
    }
}

fn state_with_projection_failure(store: SimEventStore, system_name: &str) -> ServerState {
    state_with_projection_failure_control(
        store,
        system_name,
        PARENT_IOA,
        TIMED_CHILD_WITH_RESET_IOA,
        true,
    )
    .0
}

fn state_with_projection_failure_control(
    store: SimEventStore,
    system_name: &str,
    parent_ioa: &str,
    child_ioa: &str,
    fail_writes: bool,
) -> (ServerState, Arc<AtomicBool>) {
    let csdl = parse_csdl(CSDL).expect("composite timeout CSDL parses");
    let specs = BTreeMap::from([
        ("Parent".to_string(), parent_ioa.to_string()),
        ("TimedChild".to_string(), child_ioa.to_string()),
    ]);
    let mut storage = StorageStack::from_sim(store, None);
    let fail_writes = Arc::new(AtomicBool::new(fail_writes));
    storage.query_plane = Some(Arc::new(FailingQueryPlane {
        fail_writes: fail_writes.clone(),
    }));
    let state = ServerState::with_storage_stack(
        ActorSystem::new(system_name),
        csdl,
        CSDL.to_string(),
        specs,
        storage,
    )
    .expect("composite timeout state builds");
    *state
        .query_projection_queue
        .lock()
        .expect("query projection queue lock") = None;
    (state, fail_writes)
}

#[tokio::test(start_paused = true)]
async fn atomic_composite_creation_arms_timeout_when_projection_fails_after_commit() {
    let seed = 236;
    let (_guard, clock, _ids) = temper_runtime::scheduler::install_deterministic_context(seed);
    let store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "timed-composite-projection-failure";
    let persistence_id = format!("default:TimedChild:{entity_id}");
    let state = state_with_projection_failure(store.clone(), "composite-projection-failure");

    let error = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-projection-failure",
            "CreateTimedChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "TimedChild",
                    "entity_id": entity_id,
                    "action": "Create",
                    "params": {}
                }]
            }),
            &AgentContext::for_service("composite-projection-failure-test"),
        )
        .await
        .expect_err("the injected query projection write must fail");
    assert!(
        error
            .to_string()
            .contains("query projection write failed after composite batch"),
        "unexpected post-commit failure: {error}"
    );
    assert_eq!(
        store
            .dump_journal(&persistence_id)
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "Create"],
        "the composite journal commit precedes the injected projection failure"
    );
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("TimedChild".to_string(), 1)],
        "post-commit projection failure must not strand the durable timeout"
    );

    tokio::time::advance(std::time::Duration::from_secs(60)).await;
    clock.advance_by(600);
    for _ in 0..128 {
        tokio::task::yield_now().await;
        if store
            .dump_journal(&persistence_id)
            .iter()
            .any(|event| event.event_type == "TimeoutFail")
        {
            break;
        }
    }
    assert_eq!(
        store
            .dump_journal(&persistence_id)
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "Create", "TimeoutFail"],
        "the committed target must time out without retry, access, or restart"
    );

    tokio::time::advance(std::time::Duration::from_secs(60)).await;
    clock.advance_by(600);
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        store
            .dump_journal(&persistence_id)
            .iter()
            .filter(|event| event.event_type == "TimeoutFail")
            .count(),
        1,
        "the projection-fault path must deliver the timeout exactly once"
    );
}

#[path = "composite_timeout_clock_existing_target_projection_fault_tests.rs"]
mod existing_target_tests;
