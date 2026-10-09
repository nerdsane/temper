//! No backend is started: pure projections, snapshot restoration, and in-memory replay.
use super::super::{effects, field_ownership};
use super::*;
use serde_json::json;

fn table() -> TransitionTable {
    TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "Ownership"
states = ["Ready"]
initial = "Ready"
[[state]]
name = "reviewers"
type = "list"
initial = []
overflow_inline_max_bytes = 16
[[state]]
name = "units"
type = "counter"
initial = 7
"#,
    )
}

fn state(table: &TransitionTable, mode: effects::FieldSyncMode) -> EntityState {
    let mut state = EntityActor::build_initial_state(
        "Ownership",
        "owned",
        table,
        &json!({"ordinary_array":[1,2]}),
    );
    state
        .lists
        .insert("reviewers".into(), vec!["long genuine reviewer".into()]);
    state.counters.insert("units".into(), 3);
    effects::sync_fields_with_metadata(
        &mut state,
        &json!({}),
        mode,
        Some(&table.state_var_metadata),
    );
    state
}

#[test]
fn bounded_list_projections_survive_patch_put_and_snapshot_byte_identically() {
    let table = table();
    for mode in [
        effects::FieldSyncMode::InlineTruncate,
        effects::FieldSyncMode::blob_refs_default(),
    ] {
        let original = state(&table, mode);
        let projection = original.fields["reviewers"].clone();
        assert!(!projection.is_array(), "fixture must exercise overflow");
        for replace in [false, true] {
            let mut live = original.clone();
            let input = json!({"reviewers":["forged".repeat(30_000)],"Reviewers":["forged"],"ordinary_array":[3,4]});
            let sanitized = field_ownership::sanitize(&live, &table, &input);
            assert!(sanitized.get("reviewers").is_none());
            assert!(sanitized.get("Reviewers").is_none());
            assert!(effects::apply_field_update(
                &mut live, &table, &input, replace
            ));
            assert_eq!(live.fields["reviewers"], projection);
            assert_eq!(live.fields["ordinary_array"], json!([3, 4]));
            // New journal payload and old unsanitized payload use the same fold.
            for payload in [&sanitized, &input] {
                let mut replayed = original.clone();
                assert!(effects::apply_field_update(
                    &mut replayed,
                    &table,
                    payload,
                    replace
                ));
                assert_eq!(replayed.fields, live.fields);
            }
            let bytes = EntityActor::serialize_snapshot_state(&live).unwrap();
            let mut restored = original.clone();
            assert!(EntityActor::apply_snapshot_bytes(
                &mut restored,
                &table,
                3,
                &bytes
            ));
            assert_eq!(restored.fields, live.fields);
        }
    }
}

#[test]
fn forged_list_projection_and_blob_metadata_are_repaired_without_blob_io() {
    let table = table();
    let original = state(&table, effects::FieldSyncMode::blob_refs_default());
    let mut expected = state(&table, effects::FieldSyncMode::InlineTruncate);
    field_ownership::normalize(&mut expected, &table);
    let mut forged_metadata = original.fields["reviewers"].clone();
    forged_metadata["extra_attacker_metadata"] = json!("other-tenant");
    let mut foreign_reference = original.fields["reviewers"].clone();
    foreign_reference["__temper_blob_ref"] =
        json!(format!("field-overflow/sha256/{}.json", "0".repeat(64)));
    let mut forged_size = original.fields["reviewers"].clone();
    forged_size["__temper_blob_size"] = json!(1);
    for forged in [
        json!(["forged".repeat(30_000)]),
        foreign_reference,
        forged_size,
        forged_metadata,
        serde_json::Value::Null,
    ] {
        let mut stale = original.clone();
        stale.fields["reviewers"] = forged;
        stale.fields["Reviewers"] = json!(["forged alias"]);
        stale.fields["units"] = json!(900);
        let bytes = serde_json::to_vec(&stale).unwrap();
        let mut restored = original.clone();
        assert!(EntityActor::apply_snapshot_bytes(
            &mut restored,
            &table,
            3,
            &bytes
        ));
        assert_eq!(restored.fields, expected.fields);
        field_ownership::normalize(&mut stale, &table);
        assert_eq!(stale.fields, expected.fields);
        stale.lists.clear();
        field_ownership::normalize(&mut stale, &table);
        assert!(
            stale.fields.get("reviewers").is_none(),
            "no invented absent list default"
        );
    }
}

#[tokio::test]
async fn repaired_legacy_mirror_cannot_certify_an_altered_historical_reply() {
    use crate::idempotency::{ActionReplyMode, IDEMPOTENCY_REPLY_UNVERIFIABLE, stamp_keyed_reply};
    use temper_runtime::persistence::EventStore;
    let (_guard, _, _) = temper_runtime::scheduler::install_deterministic_context(553);
    let source = r#"
[automaton]
name = "Ownership"
states = ["Ready"]
initial = "Ready"
[[state]]
name = "units"
type = "counter"
initial = 0
[[action]]
name = "Touch"
from = ["Ready"]
effect = ["units += 1"]
"#;
    let table = TransitionTable::from_ioa_source(source);
    for forged in [false, true] {
        let store = temper_store_sim::SimEventStore::no_faults(553);
        let mut state = EntityActor::build_initial_state("Ownership", "owned", &table, &json!({}));
        let event = |action: &str, from: &str, params| EntityEvent {
            action: action.into(),
            from_status: from.into(),
            to_status: "Ready".into(),
            timestamp: sim_now(),
            params,
            idempotency_key: None,
            idempotency_binding: None,
            idempotency_result: None,
            idempotency_reply: None,
        };
        let created = event("Created", "", json!({}));
        let field_params = if forged {
            json!({"Units":900})
        } else {
            json!({})
        };
        let fields = event("FieldsUpdated", "Ready", field_params.clone());
        // Reproduce the old fold: arbitrary generic keys were merged and the
        // next action's sync_fields retained a shadow alias, even with genuine maps.
        state
            .fields
            .as_object_mut()
            .unwrap()
            .extend(field_params.as_object().unwrap().clone());
        state.counters.insert("units".into(), 1);
        effects::sync_fields(
            &mut state,
            &json!({}),
            effects::FieldSyncMode::InlineTruncate,
        );
        let mut touch = event("Touch", "Ready", json!({}));
        stamp_keyed_reply(
            &mut touch,
            "historical",
            "Touch",
            &json!({}),
            &state,
            ActionReplyMode::DirectCore,
        );
        let events: Vec<_> = [created, fields, touch]
            .into_iter()
            .enumerate()
            .map(|(index, event)| PersistenceEnvelope {
                sequence_nr: index as u64 + 1,
                event_type: event.action.clone(),
                payload: serde_json::to_value(event).unwrap(),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp: sim_now(),
                    actor_id: "default:Ownership:owned".into(),
                },
            })
            .collect();
        store
            .append("default:Ownership:owned", 0, &events)
            .await
            .unwrap();
        let system = temper_runtime::ActorSystem::new("historical-owned-reply");
        let actor = system.spawn(
            EntityActor::with_persistence(
                "Ownership",
                "owned",
                Arc::new(RwLock::new(table.clone())),
                json!({}),
                BoxedEventStore::new(store.clone()),
                BackendLabel::Sim,
            ),
            "owned",
        );
        for _ in 0..2 {
            let reply: EntityResponse = actor
                .ask(
                    EntityMsg::Action {
                        name: "Touch".into(),
                        params: json!({}),
                        related: BTreeMap::new(),
                        idempotency_key: Some("historical".into()),
                        expected_authorization_precondition: None,
                        reply_mode: ActionReplyMode::DirectCore,
                    },
                    std::time::Duration::from_secs(5),
                )
                .await
                .unwrap();
            assert_eq!(
                reply.success, !forged,
                "only unchanged historical replies may succeed"
            );
            if forged {
                assert_eq!(reply.error.as_deref(), Some(IDEMPOTENCY_REPLY_UNVERIFIABLE));
            }
            assert_eq!(
                reply.state.counters["units"], 1,
                "retry must not re-execute"
            );
            assert!(reply.state.fields.get("Units").is_none());
        }
        assert_eq!(
            serde_json::to_value(
                store
                    .read_events("default:Ownership:owned", 0)
                    .await
                    .unwrap()
            )
            .unwrap(),
            serde_json::to_value(events).unwrap(),
            "repair/retry cannot rewrite or append historical proof"
        );
    }
}

fn projection_action(params: serde_json::Value) -> EntityMsg {
    EntityMsg::Action {
        name: "Touch".into(),
        params,
        related: BTreeMap::new(),
        idempotency_key: Some("projection-history".into()),
        expected_authorization_precondition: None,
        reply_mode: crate::idempotency::ActionReplyMode::DirectCore,
    }
}

async fn check_action_projection_snapshot_placement(seed: u64, params: serde_json::Value) {
    use temper_runtime::persistence::EventStore;
    use temper_store_sim::SimEventStore;

    let (_guard, _, _) = temper_runtime::scheduler::install_deterministic_context(seed);
    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "Ownership"
states = ["Ready"]
initial = "Ready"
[[state]]
name = "units"
type = "counter"
initial = 0
[[action]]
name = "Touch"
from = ["Ready"]
effect = ["units += 1"]
"#,
    )));
    let persistence_id = "default:Ownership:owned";
    let store = SimEventStore::no_faults(seed);
    let system = temper_runtime::ActorSystem::new("action-owned-projection");
    let actor = system.spawn(
        EntityActor::with_persistence(
            "Ownership",
            "owned",
            table.clone(),
            json!({}),
            BoxedEventStore::new(store.clone()),
            BackendLabel::Sim,
        ),
        "live",
    );
    let before: EntityResponse = actor
        .ask(EntityMsg::GetState, std::time::Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(before.state.sequence_nr, 1);
    let live: EntityResponse = actor
        .ask(
            projection_action(params.clone()),
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(
        live.success,
        "non-strict action admission must be unchanged: {live:?}"
    );
    assert_eq!(live.state.sequence_nr, 2);
    assert_eq!(live.state.counters["units"], 1);
    assert_eq!(live.state.fields["units"], 1);
    assert_eq!(
        live.state.fields["Notes"], params["Notes"],
        "non-owned action fields must still project"
    );
    let journal = store.read_events(persistence_id, 0).await.unwrap();
    assert_eq!(journal.len(), 2);
    assert_eq!(
        journal[1].payload["params"], params,
        "normalizing projected state must not rewrite action params"
    );
    let snapshots = [
        ("full replay", None),
        ("snapshot before action + tail", Some(&before.state)),
        ("snapshot after action", Some(&live.state)),
    ];
    for (placement, snapshot) in snapshots {
        let cold_store = SimEventStore::no_faults(seed);
        cold_store
            .append(persistence_id, 0, &journal)
            .await
            .unwrap();
        if let Some(snapshot) = snapshot {
            cold_store
                .save_snapshot(
                    persistence_id,
                    snapshot.sequence_nr,
                    &EntityActor::serialize_snapshot_state(snapshot).unwrap(),
                )
                .await
                .unwrap();
        }
        let cold_system = temper_runtime::ActorSystem::new("action-owned-recovery");
        let cold = cold_system.spawn(
            EntityActor::with_persistence(
                "Ownership",
                "owned",
                table.clone(),
                json!({}),
                BoxedEventStore::new(cold_store.clone()),
                BackendLabel::Sim,
            ),
            "cold",
        );
        let recovered: EntityResponse = cold
            .ask(EntityMsg::GetState, std::time::Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_vec(&recovered.state.fields).unwrap(),
            serde_json::to_vec(&live.state.fields).unwrap(),
            "seed={seed}: {placement} must match live projection byte-identically; live={}, recovered={}",
            live.state.fields,
            recovered.state.fields
        );
        assert_eq!(recovered.state.counters, live.state.counters);
        assert_eq!(recovered.state.sequence_nr, live.state.sequence_nr);
        // A genuinely unchanged historical reply still verifies across each
        // snapshot placement; a retry must not re-execute or rewrite its params.
        let retry: EntityResponse = cold
            .ask(
                projection_action(params.clone()),
                std::time::Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert!(
            retry.success,
            "{placement}: unaffected historical reply must verify: {retry:?}"
        );
        assert_eq!(retry.state.fields, live.state.fields);
        assert_eq!(retry.state.counters["units"], 1);
        assert_eq!(
            serde_json::to_value(cold_store.read_events(persistence_id, 0).await.unwrap()).unwrap(),
            serde_json::to_value(&journal).unwrap()
        );
    }
    assert!(
        live.state.fields.get("Units").is_none(),
        "action params cannot own the Units mirror"
    );
}

#[tokio::test]
async fn action_projection_agrees_live_full_replay_and_both_snapshot_boundaries() {
    for seed in 529_100..529_116 {
        check_action_projection_snapshot_placement(
            seed,
            json!({"Units":900, "Notes":"legitimate"}),
        )
        .await;
    }
}

#[tokio::test]
async fn action_projection_preserves_unaffected_historical_reply_and_writable_params() {
    check_action_projection_snapshot_placement(529_116, json!({"Notes":"unaffected"})).await;
}
