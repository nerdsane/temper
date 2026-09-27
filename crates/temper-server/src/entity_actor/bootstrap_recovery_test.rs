//! Committed bootstrap values and constrained recovery refusals.
use super::*;

#[tokio::test]
async fn legacy_bootstrap_after_skipped_history_remains_lenient() {
    let mut malformed = envelope(1, "Unknown", "", "Draft");
    malformed.payload = serde_json::json!({"action":42});
    let mut created = envelope(2, "Created", "", "Draft");
    created.payload["params"] = serde_json::json!({"Customer":"committed"});
    let store = BoxedEventStore::new(StaticEventStore {
        events: vec![malformed, created],
        ..Default::default()
    });
    let recovered = recover_entity_state_from_store(
        "default",
        "Order",
        "security-replay",
        &order_table(),
        &store,
        BackendLabel::Turso,
        &serde_json::json!({}),
        None,
        false,
    )
    .await
    .unwrap();
    assert_eq!(recovered.sequence_nr, 2);
    assert_eq!(recovered.fields["Customer"], "committed");
    assert!(
        recover_authoritative_entity_state_from_store(
            "default",
            "Order",
            "security-replay",
            &order_table(),
            &store,
            BackendLabel::Turso,
            &serde_json::json!({}),
            None,
        )
        .await
        .is_err()
    );
}

#[cfg(any(test, feature = "sim"))]
#[tokio::test]
async fn skipped_history_does_not_append_a_fresh_bootstrap_on_restart() {
    use std::time::Duration;
    use temper_runtime::ActorSystem;
    use temper_store_sim::SimEventStore;
    for seed in 0..8 {
        let journal = Arc::new(SimEventStore::no_faults(seed));
        let mut malformed = envelope(1, "Unknown", "", "Draft");
        malformed.payload = serde_json::json!({"action":42});
        journal
            .append("default:Order:security-replay", 0, &[malformed])
            .await
            .unwrap();
        let system = ActorSystem::new("skip-restart");
        for restart in 0..2 {
            let actor = system.spawn(
                EntityActor::with_persistence(
                    "Order",
                    "security-replay",
                    Arc::new(RwLock::new(order_table())),
                    serde_json::json!({"Customer":"uncommitted"}),
                    BoxedEventStore::from_arc(journal.clone()),
                    BackendLabel::Sim,
                ),
                format!("restart-{restart}"),
            );
            let recovered: EntityResponse = actor
                .ask(EntityMsg::GetState, Duration::from_secs(2))
                .await
                .unwrap();
            assert_eq!(
                recovered.state.sequence_nr, 1,
                "seed={seed}, restart={restart}"
            );
            assert_eq!(recovered.state.total_event_count, 0);
            assert!(recovered.state.fields.get("Customer").is_none());
            assert_eq!(
                journal
                    .read_events("default:Order:security-replay", 0)
                    .await
                    .unwrap()
                    .len(),
                1
            );
        }
    }
}

#[tokio::test]
async fn round_four_recovery_does_not_invent_new_declared_fields() {
    let table = TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "Order"
states = ["Draft"]
initial = "Draft"
strict_action_params = true
[[state]]
name = "revision"
type = "counter"
initial = "7"
[[action]]
name = "Advance"
kind = "input"
from = ["Draft"]
to = "Draft"
params = ["expected"]
constraints = [{ kind="param_equals_field", param="expected", field="revision" }]
"#,
    );
    let store = BoxedEventStore::new(StaticEventStore {
        events: vec![envelope(1, "Created", "", "Draft")],
        ..Default::default()
    });
    let recovered = recover_authoritative_entity_state_from_store(
        "default",
        "Order",
        "security-replay",
        &table,
        &store,
        BackendLabel::Turso,
        &serde_json::json!({}),
        None,
    )
    .await
    .unwrap();
    assert!(
        !recovered.counters.contains_key("revision"),
        "replay invented a declaration default"
    );
    let mut recovered = recovered;
    let refused = crate::entity_actor::effects::process_action(
        &mut recovered,
        &table,
        "Advance",
        &serde_json::json!({"expected":7}),
    );
    assert!(!refused.success, "new default became stored authority");
}

#[tokio::test]
async fn round_four_bootstrap_defaults_survive_spec_change_and_both_recovery_paths() {
    use std::time::Duration;
    use temper_runtime::ActorSystem;
    let source = r#"
[automaton]
name = "Order"
states = ["Draft"]
initial = "Draft"
strict_action_params = true
[[state]]
name = "revision"
type = "counter"
initial = "7"
[[state]]
name = "name"
type = "string"
initial = "original"
[[state]]
name = "enabled"
type = "bool"
initial = "TRUE"
[[state]]
name = "members"
type = "list"
initial = '["first"]'
[[action]]
name = "Advance"
kind = "input"
from = ["Draft"]
to = "Draft"
params = ["expected"]
constraints = [{kind="param_equals_field",param="expected",field="revision"}]
"#;
    let directory = tempfile::tempdir().unwrap();
    let journal = Arc::new(
        temper_store_turso::TursoEventStore::new(
            directory.path().join("events.db").to_str().unwrap(),
            None,
        )
        .await
        .unwrap(),
    );
    let store = BoxedEventStore::from_arc(journal);
    let system = ActorSystem::new("bootstrap-roundtrip");
    let actor = system.spawn(
        EntityActor::with_persistence(
            "Order",
            "security-replay",
            Arc::new(RwLock::new(TransitionTable::from_ioa_source(source))),
            serde_json::json!({}),
            store.clone(),
            BackendLabel::Turso,
        ),
        "original",
    );
    let initial: EntityResponse = actor
        .ask(EntityMsg::GetState, Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(initial.state.counters["revision"], 7);
    let events = store
        .read_events("default:Order:security-replay", 0)
        .await
        .unwrap();
    assert_eq!(
        events[0].payload["initial_values"]["counters"]["revision"],
        7
    );
    let changed = source
        .replace("initial = \"7\"", "initial = \"9\"")
        .replace("initial = \"original\"", "initial = \"changed\"");
    let table = TransitionTable::from_ioa_source(&changed);
    assert_eq!(
        EntityActor::build_initial_state("Order", "fresh", &table, &serde_json::json!({})).counters
            ["revision"],
        9
    );
    let recovered = recover_authoritative_entity_state_from_store(
        "default",
        "Order",
        "security-replay",
        &table,
        &store,
        BackendLabel::Turso,
        &serde_json::json!({}),
        None,
    )
    .await
    .unwrap();
    assert_eq!(recovered.counters, initial.state.counters);
    assert_eq!(recovered.booleans, initial.state.booleans);
    assert_eq!(recovered.lists, initial.state.lists);
    assert_eq!(recovered.fields, initial.state.fields);
    let snapshot = EntityActor::serialize_snapshot_state(&initial.state).unwrap();
    store
        .save_snapshot(
            "default:Order:security-replay",
            initial.state.sequence_nr,
            &snapshot,
        )
        .await
        .unwrap();
    let snapshot_recovery = recover_entity_state_from_store(
        "default",
        "Order",
        "security-replay",
        &table,
        &store,
        BackendLabel::Turso,
        &serde_json::json!({}),
        None,
        true,
    )
    .await
    .unwrap();
    assert_eq!(snapshot_recovery.counters, recovered.counters);
    assert_eq!(snapshot_recovery.fields, recovered.fields);
}

#[tokio::test]
async fn round_four_lenient_recovery_refuses_unreadable_contracted_prestate() {
    for (strict, constrained, should_fail) in [
        (true, false, true),
        (false, true, true),
        (false, false, false),
    ] {
        let mut table = order_table();
        table.strict_action_params = strict;
        if constrained {
            table = TransitionTable::from_ioa_source(
                crate::entity_actor::actor::contract_state_tests::OVERFLOW_CONTRACT,
            );
            table.strict_action_params = false;
        }
        let store = BoxedEventStore::new(StaticEventStore {
            read_error: Some("injected unavailable journal".into()),
            ..Default::default()
        });
        let recovered = recover_entity_state_from_store(
            "default",
            "Order",
            "unreadable",
            &table,
            &store,
            BackendLabel::Turso,
            &serde_json::json!({}),
            None,
            false,
        )
        .await;
        assert_eq!(
            recovered.is_err(),
            should_fail,
            "strict={strict}, constrained={constrained}"
        );
        if let Err(error) = recovered {
            assert!(error.to_string().contains("injected unavailable journal"));
        }
    }
}

#[tokio::test]
async fn closed_cached_actor_recovers_once_after_journal_outage() {
    use crate::{ServerState, StorageStack};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use temper_runtime::{ActorSystem, tenant::TenantId};
    let unavailable = Arc::new(AtomicBool::new(true));
    let mut created = envelope(1, "Created", "", "Draft");
    created.payload["params"] = serde_json::json!({"Customer":"committed"});
    let store = BoxedEventStore::new(StaticEventStore {
        events: vec![created],
        unavailable: unavailable.clone(),
        ..Default::default()
    });
    let csdl = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
    let source = ORDER_IOA.replace("[automaton]", "[automaton]\nstrict_action_params = true");
    let state = ServerState::with_storage_stack(
        ActorSystem::new("recover-closed"),
        temper_spec::csdl::parse_csdl(csdl).unwrap(),
        csdl.into(),
        BTreeMap::from([("Order".into(), source)]),
        StorageStack::new(
            BackendLabel::Turso,
            store,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        ),
    )
    .unwrap();
    let tenant = TenantId::default();
    let first = state
        .get_or_spawn_tenant_actor_with_fields(
            &tenant,
            "Order",
            "security-replay",
            serde_json::json!({}),
        )
        .unwrap();
    assert!(
        first
            .ask::<EntityResponse>(EntityMsg::GetState, Duration::from_secs(5))
            .await
            .is_err()
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while first.tell(EntityMsg::GetState).is_ok() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("failed activation must close its mailbox");
    unavailable.store(false, Ordering::SeqCst);
    let mut callers = tokio::task::JoinSet::new();
    for _ in 0..16 {
        let state = state.clone();
        let tenant = tenant.clone();
        callers.spawn(async move {
            state
                .get_or_spawn_tenant_actor_with_fields(
                    &tenant,
                    "Order",
                    "security-replay",
                    serde_json::json!({}),
                )
                .unwrap()
        });
    }
    let mut incarnations = std::collections::HashSet::new();
    while let Some(result) = callers.join_next().await {
        let actor = result.unwrap();
        assert_ne!(
            actor.id().uid,
            first.id().uid,
            "closed cache entry must be replaced"
        );
        incarnations.insert(actor.id().uid);
        let response: EntityResponse = actor
            .ask(EntityMsg::GetState, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(response.state.fields["Customer"], "committed");
    }
    assert_eq!(incarnations.len(), 1);
    assert_eq!(state.active_actor_count(), 1);
}
