//! A historical response must not act like a fresh transition in any current
//! read surface or in the reaction dispatcher.
use super::*;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use temper_runtime::persistence::PersistenceError;
use temper_server::storage::{
    BackendLabel, BoxedEventStore, QueryPlaneStore, QueryProjectionFieldsRow,
};

#[derive(Default)]
struct Projection(Mutex<BTreeMap<String, (String, u64)>>);
#[async_trait]
impl QueryPlaneStore for Projection {
    async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Option<Vec<(String, u64)>>, PersistenceError> {
        Ok(None)
    }
    async fn upsert_projection(
        &self,
        tenant: &str,
        kind: &str,
        id: &str,
        status: &str,
        _: &Value,
        _: &Value,
        seq: u64,
    ) -> Result<(), PersistenceError> {
        self.0
            .lock()
            .unwrap()
            .insert(format!("{tenant}:{kind}:{id}"), (status.into(), seq));
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

fn fixture(store: &SimEventStore, projection: &Arc<Projection>) -> ServerState {
    let spec = TASK_WITH_TIMEOUT_IOA.replace(
        "to = \"Running\"",
        r#"to = "Running"
[[action.triggers]]
name = "count_start"
kind = "entity"
target_entity = "Counter"
target_action = "Count"
resolve_target = { kind = "same_id" }
"#,
    );
    let counter = r#"
[automaton]
name = "Counter"
states = ["Idle"]
initial = "Idle"
[[action]]
name = "Count"
from = ["Idle"]
to = "Idle"
"#;
    let parent = r#"
[automaton]
name = "Parent"
states = ["Idle"]
initial = "Idle"
[[action]]
name = "Launch"
from = ["Idle"]
to = "Idle"
[[action.triggers]]
name = "start_task"
kind = "entity"
target_entity = "TimedTask"
target_action = "Start"
resolve_target = { kind = "same_id" }
"#;
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL_XML).unwrap(),
        CSDL_XML.into(),
        &[
            ("TimedTask", &spec),
            ("Counter", counter),
            ("Parent", parent),
        ],
    );
    let mut state = ServerState::from_registry(ActorSystem::new("historical-retry"), registry);
    // Inline projection writes keep all I/O inside this controlled schedule.
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
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .unwrap();
    state.rebuild_reaction_dispatcher();
    state
}

#[tokio::test(start_paused = true)]
async fn historical_idempotency_retry_preserves_current_state_and_does_not_repeat_reactions() {
    for seed in 51920..51928 {
        let (_guard, _, _) = install_deterministic_context(seed);
        let store = SimEventStore::no_faults(seed);
        let projection = Arc::new(Projection::default());
        let state = fixture(&store, &projection);
        let tenant = TenantId::default();
        let agent = AgentContext {
            idempotency_key: Some("start-once".into()),
            ..AgentContext::system()
        };
        let original = state
            .dispatch_tenant_action(&tenant, "TimedTask", "task", "Start", json!({}), &agent)
            .await
            .unwrap();
        assert!(original.success, "{original:?}");
        let done = state
            .dispatch_tenant_action(
                &tenant,
                "TimedTask",
                "task",
                "TimeoutFail",
                json!({}),
                &AgentContext::system(),
            )
            .await
            .unwrap();
        assert_eq!(done.state.status, "TimedOut");
        let before = store.total_events();
        assert_eq!(
            store
                .dump_journal("default:Counter:task")
                .iter()
                .filter(|e| e.payload["action"] == "Count")
                .count(),
            1,
            "fixture must execute the fresh reaction"
        );
        let rows = projection.0.lock().unwrap().clone();
        let cold = fixture(&store, &projection);
        let current = cold
            .get_tenant_entity_state(&tenant, "TimedTask", "task")
            .await
            .unwrap();
        assert_eq!(current.state.status, "TimedOut");
        let cache = cold.entity_state_cache.lock().unwrap().clone();
        let mut events = cold.event_tx.subscribe();
        for _ in 0..2 {
            let error = cold
                .dispatch_tenant_action(&tenant, "TimedTask", "task", "Start", json!({}), &agent)
                .await
                .expect_err("cold dispatcher history lacks final reply proof");
            assert!(
                error.contains("IdempotencyKeyUnverifiable"),
                "seed {seed}: {error}"
            );
            let current = cold
                .get_tenant_entity_state(&tenant, "TimedTask", "task")
                .await
                .unwrap();
            assert_eq!(current.state.status, "TimedOut", "actor was rewound");
            assert_eq!(
                projection.0.lock().unwrap().clone(),
                rows,
                "projection was rewound"
            );
            assert_eq!(
                cold.entity_state_cache
                    .lock()
                    .unwrap()
                    .iter()
                    .collect::<Vec<_>>(),
                cache.iter().collect::<Vec<_>>(),
                "status cache was rewound"
            );
            assert!(
                cold.state_timeout_tracker.pending_snapshot().is_empty(),
                "historical timer rearmed"
            );
            assert!(
                events.try_recv().is_err(),
                "historical transition was broadcast"
            );
            assert_eq!(
                store.total_events(),
                before,
                "seed {seed}: historical retry reran a fresh reaction"
            );
            assert!(
                !cold
                    .actor_registry
                    .read()
                    .unwrap()
                    .contains_key("default:Counter:task"),
                "seed {seed}: historical retry dispatched a fresh reaction to Counter"
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn fresh_parent_reaction_does_not_cascade_a_replayed_child() {
    let (_guard, _, _) = install_deterministic_context(51940);
    let store = SimEventStore::no_faults(51940);
    let projection = Arc::new(Projection::default());
    let state = fixture(&store, &projection);
    let tenant = TenantId::default();
    let agent = AgentContext {
        idempotency_key: Some("same-chain-key".into()),
        ..AgentContext::system()
    };
    let original = state
        .dispatch_tenant_action(
            &tenant,
            "TimedTask",
            "old-child",
            "Start",
            json!({}),
            &agent,
        )
        .await
        .unwrap();
    assert!(original.success);
    let cold = fixture(&store, &projection);
    let parent = cold
        .dispatch_tenant_action(&tenant, "Parent", "old-child", "Launch", json!({}), &agent)
        .await
        .unwrap();
    assert!(parent.success, "{parent:?}");
    assert!(
        cold.actor_registry
            .read()
            .unwrap()
            .contains_key("default:TimedTask:old-child"),
        "fixture must reach the replayed child"
    );
    assert!(
        !cold
            .actor_registry
            .read()
            .unwrap()
            .contains_key("default:Counter:old-child"),
        "a replayed child must not cascade fresh grandchild reactions"
    );
    // The same parent rule still cascades when its child really is fresh.
    let fresh = cold
        .dispatch_tenant_action(&tenant, "Parent", "new-child", "Launch", json!({}), &agent)
        .await
        .unwrap();
    assert!(fresh.success, "{fresh:?}");
    assert_eq!(
        store
            .dump_journal("default:Counter:new-child")
            .iter()
            .filter(|event| event.payload["action"] == "Count")
            .count(),
        1,
        "fresh children must preserve reaction cascades"
    );
}
