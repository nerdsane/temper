//! A timer nothing saved names a setter for fires as the principal the
//! record's journal names: the one that last moved the record by request.
//!
//! A record that entered its timed state before timer rows were kept, or
//! whose row was lost, used to be armed at boot as the kernel's state-timer
//! service. A timer whose own move resets it then fired as the service for
//! good, and every move after it that needed a person was refused (Inspiration
//! Bank's Feed, 31 Oct 2026: its reactions withheld with principal
//! service:state-timer).
//!
//! Each test sets a record polling as someone, takes away or spoils what its
//! row says, restarts the server over the same store, and reads who the
//! reaction after the next polls ran as (`Merge` binds its caller). Each
//! scenario runs across 16 seeds on a single-threaded paused clock, with
//! seeded request/crash times, poll counts and downtime. The clock and
//! journal survive each process; dropping its runtime stops every task.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use temper_authz::{PrincipalKind, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::persistence::{PersistenceError, StateTimerRecord};
use temper_runtime::scheduler::{DeterministicIdGen, DeterministicRng, install_sim_context};
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::storage::StateTimerStore;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

#[path = "dst_state_timers_follow_state/process_clock.rs"]
mod process_clock;
use process_clock::ProcessClock;

const SEEDS: u64 = 16;

/// `Start` enters Checking, whose timer fires `Look` every second and resets
/// itself. Once `looks` reaches LOOKS, a reaction to `Look` makes `Merge`,
/// which binds whoever it runs as.
const WATCH_IOA: &str = r#"
[automaton]
name = "Watch"
states = ["Idle", "Checking", "Merged"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Merged"]
strict_action_params = true

[[state]]
name = "looks"
type = "counter"
initial = 0

[[state]]
name = "merged_by"
type = "string"
initial = ""

[[action]]
name = "Start"
kind = "input"
from = ["Idle"]
to = "Checking"
params = []

[[state_timeout]]
state = "Checking"
after_seconds = 1
on_timeout = "Look"
reset_on = ["Look"]

[[action]]
name = "Look"
kind = "internal"
from = ["Checking"]
params = []
effect = ["looks += 1"]

[[action.triggers]]
name = "merge_when_ready"
kind = "entity"
target_entity = "Watch"
target_action = "Merge"
guard = "looks >= LOOKS"
resolve_target = { kind = "same_id" }

[[action]]
name = "Merge"
kind = "input"
from = ["Checking"]
to = "Merged"
params = [{ name = "merged_by", type = "string", source = "authenticated_subject" }]
"#;

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Watch" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Watch">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="looks" Type="Edm.Int64"/>
        <Property Name="merged_by" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Watches" EntityType="Temper.Watch.Watch"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const TENANT: &str = "default";

/// Timer rows committed with the journal's events, as on Turso.
struct CommittedRows(SimEventStore);

#[async_trait::async_trait]
impl StateTimerStore for CommittedRows {
    async fn upsert_state_timer(&self, row: &StateTimerRecord) -> Result<(), PersistenceError> {
        self.0.upsert_state_timer(row);
        Ok(())
    }

    async fn delete_state_timer(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> Result<(), PersistenceError> {
        self.0
            .delete_state_timer(tenant, entity_type, entity_id, arm_id);
        Ok(())
    }

    async fn list_state_timers(
        &self,
        tenant: &str,
    ) -> Result<Vec<StateTimerRecord>, PersistenceError> {
        Ok(self.0.list_state_timers(tenant))
    }
}

/// Watch's spec, merging at `looks` looks a look every `every` seconds.
fn watch_ioa(looks: u32, every: u32) -> String {
    WATCH_IOA
        .replace("LOOKS", &looks.to_string())
        .replace("after_seconds = 1", &format!("after_seconds = {every}"))
}

async fn server_looking_every(
    name: &str,
    store: &SimEventStore,
    looks: u32,
    every: u32,
) -> ServerState {
    let ioa = watch_ioa(looks, every);
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        TENANT,
        parse_csdl(CSDL).expect("CSDL should parse"),
        CSDL.to_string(),
        &[("Watch", ioa.as_str())],
    );
    let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    state.state_timer_rows = Some(Arc::new(CommittedRows(store.clone())));
    state
        .authz
        .reload_tenant_policies(TENANT, "permit(principal, action, resource);")
        .expect("policy should load");
    state.rebuild_reaction_dispatcher();
    state
        .start_runtime()
        .await
        .expect("server starts")
        .await
        .expect("boot walk completes");
    state
}

fn as_who(security_ctx: SecurityContext) -> AgentContext {
    AgentContext {
        security_ctx: Some(security_ctx),
        ..AgentContext::default()
    }
}

fn person(id: &str) -> AgentContext {
    as_who(SecurityContext::from_verified_jwt(
        id,
        PrincipalKind::Customer,
        None,
        None,
        None,
        None,
    ))
}

/// An agent acting for `person` with `role`.
fn agent_for(person: &str, role: &str) -> AgentContext {
    as_who(SecurityContext::from_verified_jwt(
        "agent-1",
        PrincipalKind::Agent,
        Some("claude-code"),
        Some(person),
        Some(role),
        None,
    ))
}

/// What becomes of the record's timer row while the server is down.
#[derive(Clone, Copy, Debug)]
enum Spoil {
    /// The row is gone, as for a record that entered its state before rows
    /// were kept.
    Lost,
    /// The row says the timer was armed for no one, as a boot that found no
    /// row left it.
    Service,
}

/// The row's `armed_by` for a timer armed as the state-timer service.
fn service_armed_by() -> String {
    let service = AgentContext::for_service("state-timer");
    json!({
        "security_ctx": service.security_ctx,
        "agent_id": service.agent_id,
        "agent_type": service.agent_type,
    })
    .to_string()
}

async fn watch(state: &ServerState, id: &str) -> (String, serde_json::Map<String, Value>) {
    let record = state
        .get_tenant_entity_state(&TenantId::new(TENANT), "Watch", id)
        .await
        .expect("the watch exists")
        .state;
    let fields = record.fields.as_object().cloned().unwrap_or_default();
    (record.status, fields)
}

/// One simulated process. Dropping its single-threaded runtime stops all
/// tasks; the journal and clock outlive it, as in the timer restart harness.
struct Process {
    runtime: tokio::runtime::Runtime,
    state: ServerState,
}

impl Process {
    fn start(clock: &ProcessClock, store: &SimEventStore, looks: u32, every: u32) -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .expect("runtime");
        let state = runtime.block_on(async {
            clock.begin();
            server_looking_every("journal-recovery", store, looks, every).await
        });
        Self { runtime, state }
    }

    fn kill(self, clock: &ProcessClock, downtime: Duration) {
        self.runtime.block_on(async { clock.end(downtime) });
        let Self { runtime, state } = self;
        drop(state);
        drop(runtime);
    }
}

/// Let all work at this simulated instant finish before inspecting it.
async fn settle() {
    for _ in 0..600 {
        tokio::task::yield_now().await;
    }
}

/// Wait on the runtime's paused clock, including every self-resetting poll.
async fn reaches(
    state: &ServerState,
    id: &str,
    status: &str,
    within: Duration,
) -> Result<serde_json::Map<String, Value>, String> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        settle().await;
        let (now, fields) = watch(state, id).await;
        if now == status {
            return Ok(fields);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("watch {id} stayed {now} with {fields:?}"));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn reaches_looks(state: &ServerState, id: &str, looks: i64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        settle().await;
        let (_, fields) = watch(state, id).await;
        if fields.get("looks").and_then(Value::as_i64).unwrap_or(0) >= looks {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "watch {id} never looked {looks} times: {fields:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Each seed varies the request time, polls before the crash, crash time and
/// downtime (including overdue timers). The invariant is that the reaction
/// binds the journal's caller after recovery, also after timer replacement;
/// a journal with no caller must never bind anyone.
fn check_recovery(by: &AgentContext, spoil: Spoil, install: bool, expected: Option<&str>) {
    for seed in 0..SEEDS {
        let clock = Arc::new(ProcessClock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        let mut rng = DeterministicRng::new(seed ^ 0x7e57_a127);
        let store = SimEventStore::no_faults(seed);
        let id = format!("watch-{seed}");
        let tenant = TenantId::new(TENANT);
        let request_at = rng.next_bound(500) as u64;
        let polls = 2 + rng.next_bound(4) as i64;
        let crash_after = rng.next_bound(500) as u64;
        let downtime = rng.next_bound(3000) as u64;
        let trace = format!(
            "seed {seed}, {spoil:?}, request at {request_at}ms, crash {crash_after}ms \
             after {polls} polls, down {downtime}ms, install {install}"
        );

        let first = Process::start(&clock, &store, 1000, 1);
        first.runtime.block_on(async {
            tokio::time::sleep(Duration::from_millis(request_at)).await;
            first
                .state
                .get_or_create_tenant_entity(&tenant, "Watch", &id, json!({}))
                .await
                .expect("the watch is created");
            let started = first
                .state
                .dispatch_tenant_action(&tenant, "Watch", &id, "Start", json!({}), by)
                .await
                .expect("started");
            assert!(started.success, "{trace}: {:?}", started.error);
            // The last row was armed by a timer, not the request itself.
            reaches_looks(&first.state, &id, polls).await;
            tokio::time::sleep(Duration::from_millis(crash_after)).await;
            settle().await;
        });
        first.kill(&clock, Duration::from_millis(downtime));

        let row = store
            .list_state_timers(TENANT)
            .into_iter()
            .find(|row| row.entity_id == id)
            .unwrap_or_else(|| panic!("{trace}: the poll left no row"));
        match spoil {
            Spoil::Lost => store.delete_state_timer(TENANT, "Watch", &id, &row.arm_id),
            Spoil::Service => store.upsert_state_timer(&StateTimerRecord {
                armed_by: service_armed_by(),
                ..row
            }),
        }

        // An install replaces a recovered live timer before it fires. Its
        // initial budget is an hour so the boot cannot fire it first.
        let second = Process::start(&clock, &store, 1, if install { 3600 } else { 1 });
        second.runtime.block_on(async {
            if install {
                settle().await;
                assert_eq!(watch(&second.state, &id).await.0, "Checking", "{trace}");
                let ioa = watch_ioa(1, 1);
                second
                    .state
                    .registry
                    .write()
                    .expect("registry lock") // ci-ok: infallible lock
                    .register_tenant(
                        TENANT,
                        parse_csdl(CSDL).expect("CSDL should parse"),
                        CSDL.to_string(),
                        &[("Watch", ioa.as_str())],
                    );
                second.state.rebuild_reaction_dispatcher();
                second.state.follow_spec_changes().await;
            }
            let result = reaches(&second.state, &id, "Merged", Duration::from_secs(20)).await;
            if let Some(expected) = expected {
                let fields = result.unwrap_or_else(|error| panic!("{trace}: {error}"));
                assert_eq!(fields["merged_by"], expected, "{trace}");
            } else {
                assert!(
                    result.is_err(),
                    "{trace}: a journal with no caller bound someone"
                );
                let (status, fields) = watch(&second.state, &id).await;
                assert_eq!(status, "Checking", "{trace}");
                assert!(
                    fields["looks"].as_i64().unwrap_or(0) > polls,
                    "{trace}: timer never fired"
                );
                assert_eq!(fields["merged_by"], "", "{trace}");
            }
        });
        second.kill(&clock, Duration::ZERO);
    }
}

#[test]
fn a_lost_rows_poll_runs_as_the_person_who_started_it() {
    check_recovery(&person("person-1"), Spoil::Lost, false, Some("person-1"));
}

#[test]
fn a_service_armed_poll_runs_as_the_person_who_started_it() {
    check_recovery(&person("person-2"), Spoil::Service, false, Some("person-2"));
}

#[test]
fn an_install_keeps_the_person_a_boot_found_in_the_journal() {
    check_recovery(&person("person-3"), Spoil::Service, true, Some("person-3"));
}

#[test]
fn a_lost_rows_poll_runs_as_the_agent_that_started_it() {
    check_recovery(
        &agent_for("person-3", "contributor"),
        Spoil::Lost,
        false,
        Some("person-3"),
    );
}

#[test]
fn a_poll_nobody_requested_still_runs_for_no_one() {
    check_recovery(&AgentContext::system(), Spoil::Lost, false, None);
}
