//! DST: one arming of a state timer commits its action at most once, however
//! many writers hold it.
//!
//! Workload, per seed: process `a` (the old one) opens records into `Ready`,
//! whose `Check` timer repeats every 60 s (a self-loop that resets its own
//! clock, like a run computer's health check), and holds others in `Waiting`,
//! whose `Expire` fires once after 90 s and leaves for `Done`. Process `b` (the
//! new one) then boots on the same journal and timer rows while `a` still
//! runs, as in a deploy's overlap: its boot arms every row again, so both
//! processes hold each arming and both fire it at the same instant. Per seed:
//!
//! - `b` reads some records before they fall due, so its actor holds them at
//!   the armed sequence and its fire meets the other's commit in the ADR-0046
//!   retry, which replays the journal and evaluates the action again; the
//!   others load at the fire, already past the arming;
//! - which process runs first at each step is drawn, so each side meets the
//!   other's commit;
//! - callers poke records (an action neither timer resets on) through either
//!   process, so a timer can find its record moved by something other than
//!   its own fire;
//! - some appends lose their answer after the write lands, as a store that
//!   retries the write reports it: a concurrency violation;
//! - after one to three budgets `a` stops, and `b` runs on alone.
//!
//! Invariants:
//! - an arming commits its action at most once: every `Check` comes a full
//!   budget after its clock started (the record's entry into `Ready` or the
//!   `Check` before), and each record has one `Expire`, from `Waiting`;
//! - every arming still fires: a `Ready` record never goes more than a budget
//!   and two steps without a `Check`, to the end, and every `Waiting` record
//!   reaches `Done`.
//!
//! Each process runs in its own single-threaded, paused-clock Tokio runtime,
//! driven one at a time by the test, and both advance by the same step. The
//! simulation clock is shared and moves with the steps, so both processes fire
//! a due timer at the same simulated instant.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use serde_json::json;
use temper_runtime::ActorSystem;
use temper_runtime::persistence::{PersistenceError, StateTimerRecord};
use temper_runtime::scheduler::{
    DeterministicIdGen, DeterministicRng, SimClock, install_sim_context,
};
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::storage::StateTimerStore;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

const SEEDS: u64 = 64;
const TENANT: &str = "fleet";
const CHECK_BUDGET: i64 = 60;
const STEP: Duration = Duration::from_secs(5);

/// `Check` and `Poke` count themselves, so each changes its record and is
/// written: a machine's move that changes nothing writes nothing
/// (`entity_actor::unrecorded`), and this test reads the journal.
const SPEC: &str = r#"
[automaton]
name = "Computer"
states = ["Idle", "Ready", "Waiting", "Done"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Done"]

[[state]]
name = "checks"
type = "counter"
initial = 0

[[state]]
name = "pokes"
type = "counter"
initial = 0

[[action]]
name = "Open"
from = ["Idle"]
to = "Ready"

[[action]]
name = "Hold"
from = ["Idle"]
to = "Waiting"

[[action]]
name = "Poke"
from = ["Ready", "Waiting"]
effect = ["pokes += 1"]

[[action]]
name = "Check"
from = ["Ready"]
effect = ["checks += 1"]

[[action]]
name = "Expire"
from = ["Waiting", "Done"]
to = "Done"

[[state_timeout]]
state = "Ready"
after_seconds = 60
on_timeout = "Check"
reset_on = ["Check"]

[[state_timeout]]
state = "Waiting"
after_seconds = 90
on_timeout = "Expire"
"#;

const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Fleet" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Computer"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
</EntityType>
<Action Name="Open" IsBound="true"><Parameter Name="bindingParameter" Type="Fleet.Computer"/></Action>
<Action Name="Hold" IsBound="true"><Parameter Name="bindingParameter" Type="Fleet.Computer"/></Action>
<Action Name="Poke" IsBound="true"><Parameter Name="bindingParameter" Type="Fleet.Computer"/></Action>
<Action Name="Check" IsBound="true"><Parameter Name="bindingParameter" Type="Fleet.Computer"/></Action>
<Action Name="Expire" IsBound="true"><Parameter Name="bindingParameter" Type="Fleet.Computer"/></Action>
<EntityContainer Name="Container"><EntitySet Name="Computers" EntityType="Fleet.Computer"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

/// The simulation clock both processes read, moved only by [`World::step`].
struct SharedClock(Mutex<DateTime<Utc>>);

impl SimClock for SharedClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }

    fn tick(&self) -> u64 {
        0
    }
}

/// Timer rows as the SQL stores keep them: one per record, a lower journal
/// sequence never replacing a higher one, removed only by the arming that
/// wrote it.
#[derive(Default)]
struct SharedRows(Mutex<BTreeMap<(String, String, String), StateTimerRecord>>);

#[async_trait::async_trait]
impl StateTimerStore for SharedRows {
    async fn upsert_state_timer(&self, timer: &StateTimerRecord) -> Result<(), PersistenceError> {
        let key = (
            timer.tenant.clone(),
            timer.entity_type.clone(),
            timer.entity_id.clone(),
        );
        let mut rows = self.0.lock().unwrap();
        if rows
            .get(&key)
            .is_none_or(|row| timer.sequence_nr >= row.sequence_nr)
        {
            rows.insert(key, timer.clone());
        }
        Ok(())
    }

    async fn delete_state_timer(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> Result<(), PersistenceError> {
        let key = (
            tenant.to_string(),
            entity_type.to_string(),
            entity_id.to_string(),
        );
        let mut rows = self.0.lock().unwrap();
        if rows.get(&key).is_some_and(|row| row.arm_id == arm_id) {
            rows.remove(&key);
        }
        Ok(())
    }

    async fn list_state_timers(
        &self,
        tenant: &str,
    ) -> Result<Vec<StateTimerRecord>, PersistenceError> {
        let rows = self.0.lock().unwrap();
        Ok(rows
            .values()
            .filter(|row| row.tenant == tenant)
            .cloned()
            .collect())
    }
}

/// What both processes share: the journal, the timer rows and the clock.
struct World {
    events: SimEventStore,
    rows: Arc<SharedRows>,
    clock: Arc<SharedClock>,
}

impl World {
    /// Move the clock one step, then let each running process catch up to
    /// it, in an order drawn from the seed: due timers fire in both at the
    /// same simulated instant.
    fn step(&self, processes: &[&Process], rng: &mut DeterministicRng) {
        *self.clock.0.lock().unwrap() += chrono::Duration::from_std(STEP).unwrap();
        let first = rng.next_bound(processes.len());
        for i in 0..processes.len() {
            processes[(first + i) % processes.len()].catch_up(STEP);
        }
    }

    fn persistence_id(id: &str) -> String {
        format!("{TENANT}:Computer:{id}")
    }
}

/// One server process on the shared world.
struct Process {
    runtime: tokio::runtime::Runtime,
    state: ServerState,
}

impl Process {
    /// Boot as `temper serve` does: specs, storage, then `start_runtime`,
    /// which arms every timer row again; its walk is awaited here.
    fn boot(world: &World, name: &str) -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap();
        let state = runtime.block_on(async {
            let mut registry = SpecRegistry::new();
            registry.register_tenant(
                TENANT,
                parse_csdl(CSDL).unwrap(),
                CSDL.to_string(),
                &[("Computer", SPEC)],
            );
            let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
            state.set_storage_stack(StorageStack::from_sim(world.events.clone(), None));
            state.state_timer_rows = Some(world.rows.clone());
            state.follow_spec_changes().await;
            let walk = state.start_runtime().await.expect("the first start");
            walk.await.unwrap();
            settle().await;
            state
        });
        Self { runtime, state }
    }

    fn catch_up(&self, by: Duration) {
        self.runtime.block_on(async {
            tokio::time::advance(by).await;
            settle().await;
        });
    }

    fn act(&self, id: &str, action: &str) -> bool {
        self.runtime.block_on(async {
            let ok = self
                .state
                .dispatch_tenant_action(
                    &TenantId::new(TENANT),
                    "Computer",
                    id,
                    action,
                    json!({}),
                    &AgentContext::default(),
                )
                .await
                .is_ok_and(|response| response.success);
            settle().await;
            ok
        })
    }

    /// Read a record, so this process's actor holds it.
    fn read(&self, id: &str) {
        self.runtime.block_on(async {
            let _ = self
                .state
                .get_tenant_entity_state(&TenantId::new(TENANT), "Computer", id)
                .await;
            settle().await;
        });
    }

    fn stop(self) {
        let Self { runtime, state } = self;
        drop(state);
        drop(runtime);
    }
}

/// Let every task that can run, run. The test task stays runnable, so the
/// paused clock never moves on its own.
async fn settle() {
    for _ in 0..600 {
        tokio::task::yield_now().await;
    }
}

/// One record's journal, as `(sequence, action, from, to, timestamp)`.
fn journal(world: &World, id: &str) -> Vec<(u64, String, String, String, DateTime<Utc>)> {
    world
        .events
        .dump_journal(&World::persistence_id(id))
        .into_iter()
        .map(|envelope| {
            let status = |key: &str| {
                envelope.payload[key]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            };
            (
                envelope.sequence_nr,
                envelope.event_type.clone(),
                status("from_status"),
                status("to_status"),
                envelope.metadata.timestamp,
            )
        })
        .collect()
}

/// The first invariant a `Ready` record's journal breaks, if any, up to `end`.
fn check_ready(world: &World, id: &str, end: DateTime<Utc>) -> Result<(), String> {
    let budget = chrono::Duration::seconds(CHECK_BUDGET);
    let slack = chrono::Duration::from_std(STEP * 2).unwrap();
    let mut clock_started = None;
    for (seq, action, from, to, at) in journal(world, id) {
        if to == "Ready" && from != "Ready" {
            clock_started = Some(at);
        }
        if action != "Check" {
            continue;
        }
        let started = clock_started.ok_or(format!("{id}: Check at seq {seq} outside Ready"))?;
        if at - started < budget {
            return Err(format!(
                "an arming committed its action twice: {id}'s Check at seq {seq} came {}s \
                 after its clock started",
                (at - started).num_seconds()
            ));
        }
        if at - started > budget + slack {
            return Err(format!(
                "an arming never fired: {id} went {}s without a Check before seq {seq}",
                (at - started).num_seconds()
            ));
        }
        clock_started = Some(at);
    }
    let started = clock_started.ok_or(format!("{id} never entered Ready"))?;
    if end - started > budget + slack {
        return Err(format!(
            "an arming never fired: {id} went {}s without a Check at the end",
            (end - started).num_seconds()
        ));
    }
    Ok(())
}

/// The first invariant a `Waiting` record's journal breaks, if any.
fn check_waiting(world: &World, id: &str) -> Result<(), String> {
    let expires: Vec<_> = journal(world, id)
        .into_iter()
        .filter(|(_, action, ..)| action == "Expire")
        .collect();
    match expires.as_slice() {
        [] => Err(format!("an arming never fired: {id} never expired")),
        [(_, _, from, ..)] if from == "Waiting" => Ok(()),
        [(seq, _, from, ..)] => Err(format!("{id}: Expire at seq {seq} from {from}")),
        more => Err(format!(
            "an arming committed its action twice: {id} expired {} times, at seqs {:?}",
            more.len(),
            more.iter().map(|(seq, ..)| *seq).collect::<Vec<_>>()
        )),
    }
}

fn run_seed(seed: u64) -> Result<(), String> {
    let epoch = Utc.with_ymd_and_hms(2026, 10, 26, 10, 0, 0).unwrap();
    let clock = Arc::new(SharedClock(Mutex::new(epoch)));
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let world = World {
        events: SimEventStore::no_faults(seed),
        rows: Arc::new(SharedRows::default()),
        clock,
    };
    let mut rng = DeterministicRng::new(seed ^ 0x0f1e_0001);
    let mut trace = Vec::new();
    let fail = |trace: &[String], detail: String| {
        format!("seed {seed}: {detail}\ntrace:\n  {}", trace.join("\n  "))
    };

    let old = Process::boot(&world, &format!("once-{seed}-a"));
    let mut ready = Vec::new();
    let mut waiting = Vec::new();
    for i in 0..3 + rng.next_bound(4) {
        let id = format!("c-{i}");
        old.runtime
            .block_on(async {
                old.state
                    .get_or_create_tenant_entity(&TenantId::new(TENANT), "Computer", &id, json!({}))
                    .await
                    .map(|_| ())
            })
            .map_err(|error| fail(&trace, format!("{id} is created: {error}")))?;
        let opens = i % 2 == 0 || rng.chance(0.3);
        let action = if opens { "Open" } else { "Hold" };
        if !old.act(&id, action) {
            return Err(fail(&trace, format!("{id} is {action}ed")));
        }
        trace.push(format!("{id}: {action} at step {i}"));
        let kind = if opens { &mut ready } else { &mut waiting };
        kind.push(id);
        world.step(&[&old], &mut rng);
    }

    // The new process boots while the old one still runs, before any timer
    // falls due, and arms every row again.
    let new = Process::boot(&world, &format!("once-{seed}-b"));
    for id in ready.iter().chain(&waiting) {
        if rng.chance(0.5) {
            new.read(id);
            trace.push(format!("b holds {id} before it falls due"));
        }
    }

    let overlap_steps = (1 + rng.next_bound(3)) * 12 + rng.next_bound(12);
    trace.push(format!("overlap of {overlap_steps} steps"));
    let all: Vec<String> = ready.iter().chain(&waiting).cloned().collect();
    for step in 0..overlap_steps {
        if rng.chance(0.15) {
            let id = &all[rng.next_bound(all.len())];
            let (who, process) = if rng.chance(0.5) {
                ("a", &old)
            } else {
                ("b", &new)
            };
            process.act(id, "Poke");
            trace.push(format!("step {step}: {who} pokes {id}"));
        }
        if rng.chance(0.1) {
            let id = &all[rng.next_bound(all.len())];
            world
                .events
                .lose_next_append_answers(&World::persistence_id(id), 1);
            trace.push(format!("step {step}: {id}'s next append loses its answer"));
        }
        world.step(&[&old, &new], &mut rng);
    }
    old.stop();
    trace.push("a stops".to_string());

    for _ in 0..48 {
        world.step(&[&new], &mut rng);
    }
    let end = world.clock.now();
    for id in &ready {
        check_ready(&world, id, end).map_err(|detail| fail(&trace, detail))?;
    }
    for id in &waiting {
        check_waiting(&world, id).map_err(|detail| fail(&trace, detail))?;
    }
    new.stop();
    Ok(())
}

#[test]
fn dst_a_timer_arming_commits_once_across_writers() {
    let mut broken: BTreeMap<String, (Vec<u64>, String)> = BTreeMap::new();
    for seed in 0..SEEDS {
        if let Err(report) = run_seed(seed) {
            let invariant = report
                .split(": ")
                .nth(1)
                .and_then(|detail| detail.split(':').next())
                .unwrap_or("unknown")
                .to_string();
            let entry = broken
                .entry(invariant)
                .or_insert_with(|| (Vec::new(), report));
            entry.0.push(seed);
        }
    }
    let summary: Vec<String> = broken
        .iter()
        .map(|(invariant, (seeds, first))| format!("{invariant}: seeds {seeds:?}\nfirst: {first}"))
        .collect();
    assert!(
        broken.is_empty(),
        "invariants broken over {SEEDS} seeds:\n\n{}",
        summary.join("\n\n")
    );
}
