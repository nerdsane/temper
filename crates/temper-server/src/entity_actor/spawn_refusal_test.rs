//! Reservation ownership through snapshots and optimistic-concurrency replay.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;
use temper_runtime::actor::ActorRef;
use temper_runtime::persistence::EventStore;
use temper_runtime::scheduler::install_deterministic_context;
use temper_store_sim::SimEventStore;

use super::{ChildLookup, SpawnRefusal};
use crate::entity_actor::{EntityActor, EntityMsg, EntityResponse};
use crate::registry::SpecRegistry;
use crate::storage::{BackendLabel, BoxedEventStore};
use crate::{ServerState, StorageStack};
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;

const JOB: &str = r#"
[automaton]
name = "Job"
states = ["Active"]
initial = "Active"

[[state]]
name = "last_run_id"
type = "string"
initial = ""

[[action]]
name = "Fire"
kind = "input"
from = ["Active"]
params = ["run_id"]
effect = ["spawn('Run', 'Configure', last_run_id, params.run_id)"]

[[state]]
name = "pings"
type = "counter"
initial = 0

# Counts itself so that it advances the record: a machine's move that changes
# nothing writes no event (`entity_actor::unrecorded`).
[[action]]
name = "Ping"
kind = "input"
from = ["Active"]
effect = ["pings += 1"]
"#;

async fn action(actor: &ActorRef<EntityMsg>, name: &str) -> EntityResponse {
    let reply: EntityResponse = actor
        .ask(
            EntityMsg::Action {
                name: name.to_owned(),
                params: serde_json::json!({"run_id": "reused"}),
                related: Default::default(),
                idempotency_key: None,
                expected_authorization_precondition: None,
                caller: None,
            },
            Duration::from_secs(5),
        )
        .await
        .expect("action reply");
    assert!(reply.success, "{name}: {:?}", reply.error);
    reply
}

#[tokio::test(start_paused = true)]
async fn dst_spawn_refusal_rechecks_reservation_ownership_after_a_conflict() {
    for seed in 0..8 {
        for reserve_again in [false, true] {
            let (_guard, _clock, _ids) = install_deterministic_context(seed);
            let store = SimEventStore::no_faults(seed);
            let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(JOB)));
            let system = ActorSystem::new("spawn-refusal-conflict");
            let spawn = |name| {
                system.spawn(
                    EntityActor::with_persistence(
                        "Job",
                        "job",
                        table.clone(),
                        serde_json::json!({}),
                        BoxedEventStore::new(store.clone()),
                        BackendLabel::Sim,
                    ),
                    name,
                )
            };
            let stale = spawn("stale");
            // No dispatcher runs here: the actor returns its spawn request,
            // letting us deliver that request's refusal after another writer.
            let first = action(&stale, "Fire").await;
            let request = &first.spawn_requests[0];
            let mut snapshot = serde_json::to_value(&first.state).unwrap();
            snapshot["events"] = serde_json::json!([]);
            snapshot["events_since_snapshot"] = serde_json::json!(0);
            snapshot["last_snapshot_sequence_nr"] = first.state.sequence_nr.into();
            store
                .save_snapshot(
                    "default:Job:job",
                    first.state.sequence_nr,
                    &serde_json::to_vec(&snapshot).unwrap(),
                )
                .await
                .unwrap();

            let newer = spawn("newer");
            if reserve_again {
                let next = action(&newer, "Fire").await;
                assert_ne!(
                    request.reservation_id,
                    next.spawn_requests[0].reservation_id
                );
            }
            // Advancing the parent alone must not invalidate a reservation.
            action(&newer, "Ping").await;
            let refused: EntityResponse = stale
                .ask(
                    EntityMsg::RecordSpawnRefusal {
                        refusal: SpawnRefusal {
                            reservation_id: request.reservation_id.clone(),
                            child_type: request.entity_type.clone(),
                            child_id: request.entity_id.clone(),
                            field: request.store_id_in.clone(),
                            child: ChildLookup::Absent,
                            error: "initializer refused before the newer writer".to_owned(),
                        },
                    },
                    Duration::from_secs(5),
                )
                .await
                .expect("refusal reply");
            assert!(refused.success, "seed {seed}: {:?}", refused.error);
            let cleared = refused.state.events.back().unwrap().params["cleared_field"].is_string();
            assert_eq!(
                cleared, !reserve_again,
                "seed {seed}: reservation ownership after conflict"
            );
            assert_eq!(
                refused.state.spawn_reservations.contains_key("last_run_id"),
                reserve_again
            );
            if reserve_again {
                assert_eq!(refused.state.fields["last_run_id"], "reused", "seed {seed}");
            } else {
                assert_eq!(refused.state.fields["last_run_id"], "", "seed {seed}");
            }
            let replayed: EntityResponse = spawn("replayed")
                .ask(EntityMsg::GetState, Duration::from_secs(5))
                .await
                .unwrap();
            assert_eq!(
                replayed.state.fields, refused.state.fields,
                "seed {seed}: snapshot plus tail"
            );
            assert_eq!(
                replayed.state.spawn_reservations,
                refused.state.spawn_reservations
            );
        }
    }
}

fn server(store: SimEventStore, job: &str) -> ServerState {
    let csdl = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(csdl).unwrap(),
        csdl.to_owned(),
        &[("Job", job)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new("spawn-refusal"), registry);
    state.set_storage_stack(StorageStack::from_sim(store, None));
    state
}

fn refusal(response: &EntityResponse) -> SpawnRefusal {
    let request = &response.spawn_requests[0];
    SpawnRefusal {
        reservation_id: request.reservation_id.clone(),
        child_type: request.entity_type.clone(),
        child_id: request.entity_id.clone(),
        field: request.store_id_in.clone(),
        child: ChildLookup::Absent,
        error: "initializer refused".to_owned(),
    }
}

/// Seed 0 regresses a stored null hiding the declaration from later guards.
#[tokio::test(start_paused = true)]
async fn dst_spawn_refusal_restores_the_declared_default_and_allows_a_guarded_action() {
    for (seed, replay) in (0..32).flat_map(|seed| [false, true].map(move |replay| (seed, replay))) {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let store = SimEventStore::no_faults(seed);
        let job = JOB.replace(
            "params = [\"run_id\"]",
            "params = [\"run_id\"]\nguard = \"last_run_id == ''\"",
        );
        let state = server(store.clone(), &job);
        let tenant = TenantId::default();
        let actor = state
            .get_or_spawn_tenant_actor(&tenant, "Job", "job")
            .unwrap();
        let first = action(&actor, "Fire").await;
        let response = state
            .record_spawn_refusal(&tenant, "Job", "job", refusal(&first))
            .await
            .unwrap();
        assert_eq!(response.state.fields["last_run_id"], "", "seed {seed}");
        let journal = store.dump_journal("default:Job:job");
        let params = &journal.last().unwrap().payload["params"];
        assert_eq!(params["cleared_field"], "last_run_id");
        assert_eq!(params["restored_value"], "", "seed {seed}: durable default");
        // Exercise the guard on live state and independently replayed state.
        let restarted = server(store.clone(), &job);
        let replayed = restarted
            .get_tenant_entity_state(&tenant, "Job", "job")
            .await
            .unwrap();
        assert_eq!(
            replayed.state.fields["last_run_id"], "",
            "seed {seed}: replay"
        );
        let next_actor = if replay {
            restarted
                .get_or_spawn_tenant_actor(&tenant, "Job", "job")
                .unwrap()
        } else {
            actor
        };
        action(&next_actor, "Fire").await;
    }
}

/// An ask timeout does not cancel the actor's in-flight append. Seed 0 must
/// leave exactly one durable refusal even when that append finishes late.
#[tokio::test(start_paused = true)]
async fn dst_spawn_refusal_timeout_does_not_append_again() {
    for seed in 0..32 {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let store = SimEventStore::no_faults(seed);
        let mut state = server(store.clone(), JOB);
        state.action_dispatch_timeout = Duration::from_millis(5);
        let tenant = TenantId::default();
        let actor = state
            .get_or_spawn_tenant_actor(&tenant, "Job", "job")
            .unwrap();
        let first = action(&actor, "Fire").await;
        let before = first.state.events_since_snapshot;
        let mut rng = temper_store_sim::DeterministicRng::new(seed);
        store.inject_append_delay(
            "default:Job:job",
            Duration::from_millis(75 + rng.next_u64() % 25),
        );
        let result = state
            .record_spawn_refusal(&tenant, "Job", "job", refusal(&first))
            .await;
        // Let the original append and any mistakenly queued retries finish.
        tokio::time::sleep(Duration::from_secs(1)).await;
        let settled: EntityResponse = actor
            .ask(EntityMsg::GetState, Duration::from_secs(5))
            .await
            .unwrap();
        let journal = store.dump_journal("default:Job:job");
        assert_eq!(
            journal
                .iter()
                .filter(|e| e.event_type == "SpawnRefused")
                .count(),
            1,
            "seed {seed}: a timeout must not duplicate the durable refusal"
        );
        assert!(
            result.is_err(),
            "seed {seed}: report the single attempt's timeout"
        );
        assert_eq!(
            settled.state.events_since_snapshot,
            before + 1,
            "seed {seed}: event budget"
        );
        assert!(!settled.state.spawn_reservations.contains_key("last_run_id"));
    }
}
