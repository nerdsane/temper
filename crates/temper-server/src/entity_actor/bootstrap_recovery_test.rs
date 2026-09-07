//! Committed bootstrap values and constrained recovery refusals.
use super::*;

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
