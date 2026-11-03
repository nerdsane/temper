//! DST: a parent never keeps the id of a child the kernel could not create.
//!
//! A `spawn(...)` effect writes the child's id into the parent's field as part
//! of the parent's transition, and the kernel creates the child after that
//! transition commits (ADR-0015). The child's create can still be refused: its
//! initial action's parameters fail their contract, or the store fails while
//! the child is created. The workload is a scheduled job that fires runs, like
//! a CronJob spawning Sessions: each fire spawns a `Run` whose `Configure`
//! requires a nonempty `user_message`. Seeded choices give some fires a
//! message, some an empty one, and some a store that fails reads of the new
//! child. Some jobs let each fire settle before the next, as a schedule does;
//! others fire back to back, so a refusal can reach the parent after a newer
//! spawn replaced the id.
//!
//! Invariants, checked once every spawn has settled:
//! 1. a parent's spawn field names a child that exists, is empty, or names a
//!    child whose refusal says the kernel could not look it up (the id is kept
//!    on uncertainty, never cleared on a guess);
//! 2. every child that was not created is named by a `SpawnRefused` event in
//!    its parent's journal, so the refusal is visible after the fact;
//! 3. when a job's newest run was created, the job's field names it: a late
//!    refusal of an older run does not clear a newer spawn's id.

use std::time::Duration;

use temper_runtime::ActorSystem;
use temper_runtime::scheduler::install_deterministic_context;
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::{DeterministicRng, SimEventStore};

const SEEDS: u64 = 32;

/// How long one spawn may take to settle. A settled spawn has a child or a
/// refusal on its parent. Include the base branch's backoff for pending child
/// writes whose reads fail (1 + 2 + 4 seconds for this workload). Tests pause
/// Tokio's clock, so the wait advances simulated time.
const SETTLE_POLLS: usize = 1000;
const SETTLE_POLL_INTERVAL: Duration = Duration::from_millis(10);

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.SpawnChild" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Job">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
        <Property Name="last_run_id" Type="Edm.String"/>
        <Property Name="user_message" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Run">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
        <Property Name="user_message" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Jobs" EntityType="Temper.SpawnChild.Job"/>
        <EntitySet Name="Runs" EntityType="Temper.SpawnChild.Run"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// A scheduled job: each `Fire` spawns a `Run` and stores its id.
const JOB_IOA: &str = r#"
[automaton]
name = "Job"
states = ["Active"]
initial = "Active"

[[state]]
name = "last_run_id"
type = "string"
initial = ""

[[state]]
name = "user_message"
type = "string"
initial = ""

[[action]]
name = "Fire"
kind = "input"
from = ["Active"]
params = ["run_id", "user_message"]
effect = ["spawn('Run', 'Configure', last_run_id, params.run_id, copy(user_message))"]
"#;

/// A run refuses to start without a message, as a Session does.
const RUN_IOA: &str = r#"
[automaton]
name = "Run"
states = ["Pending", "Running"]
initial = "Pending"

[[state]]
name = "user_message"
type = "string"
initial = ""

[[action]]
name = "Configure"
kind = "input"
from = ["Pending"]
to = "Running"
params = ["user_message"]

[[action.constraints]]
kind = "param_nonempty"
param = "user_message"
"#;

/// What one fire hands its run.
#[derive(Debug, Clone, Copy)]
enum Fire {
    /// A message: the run is created and starts.
    Message,
    /// An empty message: the run's `Configure` contract refuses it.
    EmptyMessage,
    /// A message, but the store fails this many reads of the new run.
    StoreFails(usize),
}

fn state_with_sim_store(seed: u64) -> (ServerState, SimEventStore) {
    let sim_store = SimEventStore::no_faults(seed);
    (state_with_store(sim_store.clone()), sim_store)
}

fn state_with_store(sim_store: SimEventStore) -> ServerState {
    state_with_specs(sim_store, JOB_IOA, RUN_IOA)
}

fn state_with_specs(sim_store: SimEventStore, job: &str, run: &str) -> ServerState {
    let csdl = parse_csdl(CSDL).expect("CSDL parses");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        csdl,
        CSDL.to_string(),
        &[("Job", job), ("Run", run)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new("dst-spawn-child"), registry);
    state.set_storage_stack(StorageStack::from_sim(sim_store.clone(), None));
    state
}

fn run_exists(store: &SimEventStore, run_id: &str) -> bool {
    !store
        .dump_journal(&format!("default:Run:{run_id}"))
        .is_empty()
}

/// The `SpawnRefused` params a job's journal records, in order.
fn refusals(store: &SimEventStore, job_id: &str) -> Vec<serde_json::Value> {
    store
        .dump_journal(&format!("default:Job:{job_id}"))
        .into_iter()
        .filter(|envelope| envelope.event_type == "SpawnRefused")
        .map(|envelope| envelope.payload["params"].clone())
        .collect()
}

/// The run ids a job's journal records as refused spawns.
fn refused_runs(store: &SimEventStore, job_id: &str) -> Vec<String> {
    refusals(store, job_id)
        .iter()
        .filter_map(|params| params["child_id"].as_str().map(str::to_owned))
        .collect()
}

/// Whether the job's journal records that the kernel could not look `run_id`
/// up when it refused it, so the job kept the id.
fn refused_unreadable(store: &SimEventStore, job_id: &str, run_id: &str) -> bool {
    refusals(store, job_id)
        .iter()
        .any(|params| params["child_id"] == run_id && params["child"] == "unreadable")
}

async fn wait_until_settled(store: &SimEventStore, job_id: &str, run_id: &str) {
    for _ in 0..SETTLE_POLLS {
        if run_exists(store, run_id) || refused_runs(store, job_id).iter().any(|id| id == run_id) {
            return;
        }
        tokio::time::sleep(SETTLE_POLL_INTERVAL).await;
    }
}

#[tokio::test(start_paused = true)]
async fn dst_a_parent_never_keeps_the_id_of_a_child_that_was_not_created() {
    for seed in 0..SEEDS {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let (state, store) = state_with_sim_store(seed);
        let tenant = TenantId::default();
        let agent = AgentContext::for_service("dst-spawn-child");
        let mut rng = DeterministicRng::new(seed);

        let jobs = 3 + (rng.next_u64() % 3) as usize;
        let mut fired: Vec<(String, String, Fire)> = Vec::new();
        let mut newest: Vec<(String, String)> = Vec::new();
        for job in 0..jobs {
            let job_id = format!("job-{seed}-{job}");
            let settle_each = rng.next_u64().is_multiple_of(2);
            state
                .get_or_create_tenant_entity(&tenant, "Job", &job_id, serde_json::json!({}))
                .await
                .expect("create Job");
            let fires = 1 + (rng.next_u64() % 3) as usize;
            for fire in 0..fires {
                let run_id = format!("run-{seed}-{job}-{fire}");
                let kind = match rng.next_u64() % 3 {
                    0 => Fire::Message,
                    1 => Fire::EmptyMessage,
                    _ => Fire::StoreFails(1 + (rng.next_u64() % 3) as usize),
                };
                let message = match kind {
                    Fire::EmptyMessage => "",
                    Fire::Message | Fire::StoreFails(_) => "summarise the day",
                };
                if let Fire::StoreFails(reads) = kind {
                    store.fail_next_reads(&format!("default:Run:{run_id}"), reads);
                }
                let response = state
                    .dispatch_tenant_action(
                        &tenant,
                        "Job",
                        &job_id,
                        "Fire",
                        serde_json::json!({ "run_id": run_id, "user_message": message }),
                        &agent,
                    )
                    .await
                    .expect("Fire dispatches");
                assert!(
                    response.success,
                    "seed {seed}: Fire refused: {:?}",
                    response.error
                );
                if settle_each {
                    wait_until_settled(&store, &job_id, &run_id).await;
                }
                fired.push((job_id.clone(), run_id, kind));
            }
            newest.push((job_id.clone(), fired.last().expect("a job fires").1.clone()));
        }
        for (job_id, run_id, _) in &fired {
            wait_until_settled(&store, job_id, run_id).await;
        }

        let restarted = state_with_store(store.clone());
        for (job_id, newest_run) in &newest {
            let live = state
                .get_tenant_entity_state(&tenant, "Job", job_id)
                .await
                .expect("read Job")
                .state;
            let stored = live.fields["last_run_id"]
                .as_str()
                .expect("a spawn field must retain its declared string type")
                .to_owned();
            assert!(
                stored.is_empty()
                    || run_exists(&store, &stored)
                    || refused_unreadable(&store, job_id, &stored),
                "seed {seed}: {job_id}.last_run_id names {stored}, which was never created"
            );
            if run_exists(&store, newest_run) {
                assert_eq!(
                    &stored, newest_run,
                    "seed {seed}: {job_id} lost the id of its newest run, which exists"
                );
            }
            let replayed = restarted
                .get_tenant_entity_state(&tenant, "Job", job_id)
                .await
                .expect("replay Job")
                .state;
            assert_eq!(
                replayed.spawn_reservations, live.spawn_reservations,
                "seed {seed}: replay must preserve reservation ownership"
            );
            assert_eq!(
                replayed.fields["last_run_id"], live.fields["last_run_id"],
                "seed {seed}: {job_id}'s spawn field must survive replay"
            );
        }
        for (job_id, run_id, kind) in &fired {
            if run_exists(&store, run_id) {
                continue;
            }
            assert!(
                refused_runs(&store, job_id).contains(run_id),
                "seed {seed}: {run_id} ({kind:?}) was not created and {job_id}'s journal \
                 does not say so"
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn dst_existing_child_survives_unreadable_journal() {
    for seed in 100..108 {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let (state, store) = state_with_sim_store(seed);
        let tenant = TenantId::default();
        state
            .dispatch_tenant_action(
                &tenant,
                "Run",
                "existing",
                "Configure",
                serde_json::json!({"user_message": "hello"}),
                &AgentContext::for_service("test"),
            )
            .await
            .expect("create existing child");
        assert!(run_exists(&store, "existing"));
        // Refuse an undeclared initializer before reading the existing child.
        // A fresh server has no resident child, and the refusal's durable
        // existence lookup fails: it must record uncertainty and keep the id.
        let job = JOB_IOA.replace("'Configure'", "'Undeclared'");
        let run = RUN_IOA.replace(
            r#"initial = "Pending""#,
            "initial = \"Pending\"\nstrict_action_params = true",
        );
        let fresh = state_with_specs(store.clone(), &job, &run);
        store.fail_next_reads("default:Run:existing", 5);
        let response = fresh
            .dispatch_tenant_action(
                &tenant,
                "Job",
                "job",
                "Fire",
                serde_json::json!({"run_id": "existing", "user_message": ""}),
                &AgentContext::for_service("test"),
            )
            .await
            .expect("fire");
        assert!(response.success);
        for _ in 0..SETTLE_POLLS {
            if !refused_runs(&store, "job").is_empty() {
                break;
            }
            tokio::time::sleep(SETTLE_POLL_INTERVAL).await;
        }
        assert_eq!(refused_runs(&store, "job"), vec!["existing"]);
        assert!(refused_unreadable(&store, "job", "existing"));
        let fields = fresh
            .get_tenant_entity_state(&tenant, "Job", "job")
            .await
            .unwrap()
            .state
            .fields;
        assert_eq!(
            fields["last_run_id"], "existing",
            "seed {seed}: unreadability must not clear a durable child"
        );
    }
}

/// An id identifies a child, not the reservation a delayed refusal cancels.
/// Seed 0 is a permanent regression for the reused-id race.
#[tokio::test(start_paused = true)]
async fn dst_delayed_refusal_keeps_a_newer_reservation_of_the_same_child_id() {
    for same_transition in [false, true] {
        let mut delayed_absences = 0;
        for seed in 0..SEEDS {
            let (_guard, _clock, _ids) = install_deterministic_context(seed);
            let _faults = temper_runtime::buggify::install_buggify_context(seed);
            let store = SimEventStore::no_faults(seed);
            let mut job = JOB_IOA.to_owned();
            let mut run = RUN_IOA.to_owned();
            if same_transition {
                job = job.replace(
                    "effect = [",
                    "effect = [\"spawn('Run', 'Undeclared', last_run_id, params.run_id)\", ",
                );
                run = run.replace(
                    "initial = \"Pending\"",
                    "initial = \"Pending\"\nstrict_action_params = true",
                );
            }
            let state = state_with_specs(store.clone(), &job, &run);
            let tenant = TenantId::default();
            let agent = AgentContext::for_service("dst-spawn-child-reused-id");
            let mut rng = DeterministicRng::new(seed);
            let job_id = format!("reuse-{seed}");
            let run_id = format!("same-run-{seed}");
            state
                .get_or_create_tenant_entity(&tenant, "Job", &job_id, serde_json::json!({}))
                .await
                .expect("create Job");
            // Keep the valid child's creation behind the older refusal's lookup.
            store.inject_append_delay(
                &format!("default:Run:{run_id}"),
                Duration::from_millis(50 + rng.next_u64() % 50),
            );
            let messages: &[&str] = if same_transition {
                &["summarise the day"]
            } else {
                &["", "summarise the day"]
            };
            for message in messages {
                let fired = state
                    .dispatch_tenant_action(
                        &tenant,
                        "Job",
                        &job_id,
                        "Fire",
                        serde_json::json!({"run_id": run_id, "user_message": message}),
                        &agent,
                    )
                    .await
                    .expect("Fire dispatches");
                assert!(fired.success, "seed {seed}: Fire failed: {:?}", fired.error);
            }
            for _ in 0..SETTLE_POLLS {
                if run_exists(&store, &run_id) && !refusals(&store, &job_id).is_empty() {
                    break;
                }
                tokio::time::sleep(SETTLE_POLL_INTERVAL).await;
            }
            assert!(
                run_exists(&store, &run_id),
                "seed {seed}: valid child must exist"
            );
            let journal = store.dump_journal(&format!("default:Job:{job_id}"));
            let newest_fire = journal
                .iter()
                .rev()
                .find(|e| e.event_type == "Fire")
                .unwrap();
            let refusal = journal
                .iter()
                .find(|e| e.event_type == "SpawnRefused")
                .unwrap();
            if refusal.payload["params"]["child"] == "absent"
                && refusal.sequence_nr > newest_fire.sequence_nr
            {
                delayed_absences += 1;
            }
            let live = state
                .get_tenant_entity_state(&tenant, "Job", &job_id)
                .await
                .expect("read live Job")
                .state;
            assert_eq!(
                live.fields["last_run_id"], run_id,
                "seed {seed}: delayed refusal cleared a newer reservation of the same id"
            );
            let restarted = state_with_specs(store.clone(), &job, &run);
            let replayed = restarted
                .get_tenant_entity_state(&tenant, "Job", &job_id)
                .await
                .expect("replay Job")
                .state;
            assert_eq!(
                replayed.spawn_reservations, live.spawn_reservations,
                "seed {seed}: replay must preserve reservation ownership"
            );
            assert_eq!(
                replayed.fields["last_run_id"], live.fields["last_run_id"],
                "seed {seed}: replay must preserve the newer reservation"
            );
        }
        assert!(
            delayed_absences > 0,
            "the workload must delay an absence past the newer reservation"
        );
    }
}
