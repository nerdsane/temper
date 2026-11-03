//! A background module call whose trigger names an `on_failure` always ends
//! in an answer, even when the server restarts while the call is in flight.
//!
//! A `Lookup` record's `Ask` starts the `lookup` module in the background;
//! its trigger answers `Finish` on success and `GiveUp` on failure, with a
//! 30-second deadline (`timeout_secs`). The real WASM engine runs the module.
//! Each server runs in its own single-threaded, paused-clock Tokio runtime;
//! dropping the runtime is the process exit. The journal and the scheduled
//! action rows live in the simulated store and outlive the process.
//!
//! - A call whose answer is still being written when the process stops is
//!   answered after the restart: once its deadline passes, the record takes
//!   `GiveUp`, as whoever asked.
//! - A call that answers leaves nothing behind: no row, and no `GiveUp` after
//!   a restart past its deadline. Neither does a call whose answer the record
//!   refuses: the call has ended.
//!
//! Engine-boundary exception: the real Wasmtime worker runs outside the
//! simulator. The bounded wall-clock waits below give it CPU while keeping
//! Tokio's virtual clock paused; all storage faults, retries and restarts use
//! explicit virtual time. These waits prove integration, not DST replay.

#[path = "dst_state_timers_follow_state/process_clock.rs"]
mod process_clock;

#[path = "lost_call_answers/recovery.rs"]
mod recovery;

use std::sync::Arc;
use std::time::Duration;

use process_clock::ProcessClock;
use serde_json::json;
use temper_runtime::ActorSystem;
use temper_runtime::persistence::EventStore;
use temper_runtime::scheduler::{DeterministicIdGen, install_sim_context};
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

const TENANT: &str = "lost-calls";
const ID: &str = "lookup-1";
const JOURNAL: &str = "lost-calls:Lookup:lookup-1";

/// The call's deadline, as its trigger declares it.
const DEADLINE_SECS: u64 = 30;

const LOOKUP: &str = r#"
[automaton]
name = "Lookup"
states = ["Idle", "Asked", "Done", "GaveUp"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Asked", "Done", "GaveUp"]

[[action]]
name = "Ask"
kind = "input"
from = ["Idle"]
to = "Asked"

[[action.triggers]]
name = "lookup"
kind = "wasm"
module = "lookup"
on_success = "Finish"
on_failure = "GiveUp"

[action.triggers.config]
timeout_secs = "30"

[[action]]
name = "Finish"
kind = "internal"
from = ["Asked"]
to = "Done"
params = [{ name = "score", type = "int" }]
guard = "params.score >= 50"

[[action]]
name = "GiveUp"
kind = "internal"
from = ["Asked"]
to = "GaveUp"
"#;

const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Lookups" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Lookup"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
</EntityType>
<EntityContainer Name="Container"><EntitySet Name="Lookups" EntityType="Lookups.Lookup"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

/// A module that answers `result` whatever it is asked.
fn answering(result: &serde_json::Value) -> String {
    let payload = result.to_string();
    let data = payload
        .bytes()
        .map(|byte| format!("\\{byte:02x}"))
        .collect::<String>();
    format!(
        r#"(module
        (import "env" "host_set_result" (func $result (param i32 i32)))
        (memory (export "memory") 1)
        (data (i32.const 0) "{data}")
        (func (export "run") (param i32 i32) (result i32)
            i32.const 0 i32.const {} call $result i32.const 0))"#,
        payload.len()
    )
}

/// One server process over `store`, its module answering `answer`.
struct Process {
    runtime: tokio::runtime::Runtime,
    state: ServerState,
}

impl Process {
    fn start(
        store: &SimEventStore,
        clock: &ProcessClock,
        name: &str,
        answer: &serde_json::Value,
    ) -> Self {
        Self::start_with_modules(store, clock, name, LOOKUP, &[("lookup", answer.clone())])
    }

    fn start_with_modules(
        store: &SimEventStore,
        clock: &ProcessClock,
        name: &str,
        source: &str,
        modules: &[(&str, serde_json::Value)],
    ) -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap();
        let state = runtime.block_on(async {
            clock.begin();
            let mut registry = SpecRegistry::new();
            let csdl = parse_csdl(CSDL).unwrap();
            registry.register_tenant(TENANT, csdl, CSDL.to_string(), &[("Lookup", source)]);
            let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
            state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
            let tenant = TenantId::new(TENANT);
            for (module, answer) in modules {
                let hash = state
                    .wasm_engine
                    .compile_and_cache(answering(answer).as_bytes())
                    .expect("the module compiles");
                state.register_wasm_module(&tenant, module, &hash);
            }
            let walk = state.start_runtime().await.expect("the first start");
            walk.await.unwrap();
            state
        });
        Self { runtime, state }
    }

    /// Exit: every task of this process stops, its writes in flight with it.
    fn kill(self, clock: &ProcessClock, downtime: Duration) {
        self.runtime.block_on(async { clock.end(downtime) });
        let Self { runtime, state } = self;
        drop(state);
        drop(runtime);
    }
}

/// Create the record and send `Ask`, as the system.
async fn ask(state: &ServerState) {
    let tenant = TenantId::new(TENANT);
    state
        .get_or_create_tenant_entity(&tenant, "Lookup", ID, json!({}))
        .await
        .expect("the record is created");
    let asked = state
        .dispatch_tenant_action(
            &tenant,
            "Lookup",
            ID,
            "Ask",
            json!({}),
            &AgentContext::system(),
        )
        .await
        .expect("Ask dispatches");
    assert!(asked.success, "Ask is taken: {:?}", asked.error);
}

/// Let the module's blocking thread and the tasks it wakes run, without
/// letting the paused clock jump: until `done` holds or five seconds of real
/// time pass. Says whether `done` held.
async fn run_until(mut done: impl FnMut() -> bool) -> bool {
    for _ in 0..5_000 {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1)); // determinism-ok: real WASM blocking worker needs CPU; virtual time must stay paused
        tokio::task::yield_now().await;
    }
    done()
}

/// The actions of the record's journal, in order.
async fn journal(store: &SimEventStore) -> Vec<String> {
    store
        .read_events(JOURNAL, 0)
        .await
        .expect("the journal reads")
        .into_iter()
        .map(|event| event.event_type)
        .collect()
}

/// Boot a second process, let it run well past the call's deadline, and
/// return the record's status as it reads it from the store.
fn status_after_restart(store: &SimEventStore, clock: &ProcessClock) -> String {
    let second = Process::start(store, clock, "lost-calls-b", &json!({}));
    let status = second.runtime.block_on(async {
        tokio::time::sleep(Duration::from_secs(4 * DEADLINE_SECS)).await;
        for _ in 0..600 {
            tokio::task::yield_now().await;
        }
        second
            .state
            .get_tenant_entity_state(&TenantId::new(TENANT), "Lookup", ID)
            .await
            .expect("the record reads")
            .state
            .status
    });
    second.kill(clock, Duration::ZERO);
    status
}

#[test]
fn a_call_in_flight_at_a_restart_is_answered_by_its_on_failure() {
    let clock = Arc::new(ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(7)));
    let store = SimEventStore::no_faults(7);
    let first = Process::start(
        &store,
        &clock,
        "lost-calls-a",
        &json!({ "params": { "score": 70 } }),
    );
    first.runtime.block_on(async {
        let tenant = TenantId::new(TENANT);
        first
            .state
            .get_or_create_tenant_entity(&tenant, "Lookup", ID, json!({}))
            .await
            .expect("the record is created");
        // The record's next write is Ask's and lands at once; the one after,
        // the module's answer, is still being written an hour later. The
        // paused clock does not move while the test waits on it.
        store.inject_append_delay(JOURNAL, Duration::ZERO);
        store.inject_append_delay(JOURNAL, Duration::from_secs(3600));
        let asked = first
            .state
            .dispatch_tenant_action(
                &tenant,
                "Lookup",
                ID,
                "Ask",
                json!({}),
                &AgentContext::system(),
            )
            .await
            .expect("Ask dispatches");
        assert!(asked.success, "Ask is taken: {:?}", asked.error);
        assert!(
            run_until(|| !store.append_faults_pending(JOURNAL)).await,
            "the module answered and its answer's write began"
        );
    });
    // The process stops with the answer unwritten: the call is lost.
    first.kill(&clock, Duration::from_secs(10));
    let after_stop = first_journal(&store);
    assert_eq!(
        after_stop.last().map(String::as_str),
        Some("Ask"),
        "{after_stop:?}"
    );

    let status = status_after_restart(&store, &clock);
    let events = first_journal(&store);
    assert_eq!(
        status, "GaveUp",
        "a call lost to a restart is answered by its on_failure; journal {events:?}"
    );
    assert_eq!(
        events.last().map(String::as_str),
        Some("GiveUp"),
        "{events:?}"
    );
    let rows = block(store.list_scheduled_actions(TENANT)).unwrap();
    assert!(rows.is_empty(), "the answer removes its deadline: {rows:?}");
}

#[test]
fn a_call_that_answers_leaves_no_deadline_behind() {
    let clock = Arc::new(ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(8)));
    let store = SimEventStore::no_faults(8);
    let first = Process::start(
        &store,
        &clock,
        "lost-calls-a",
        &json!({ "params": { "score": 70 } }),
    );
    first.runtime.block_on(async {
        ask(&first.state).await;
        let answered = run_until(|| {
            store
                .dump_journal(JOURNAL)
                .iter()
                .any(|event| event.event_type == "Finish")
        })
        .await;
        assert!(answered, "the module's answer is taken");
        let rows = store.list_scheduled_actions(TENANT).await.unwrap();
        assert!(
            rows.is_empty(),
            "the answer removes the call's deadline: {rows:?}"
        );
    });
    first.kill(&clock, Duration::from_secs(10));

    let status = status_after_restart(&store, &clock);
    let events = first_journal(&store);
    assert_eq!(status, "Done", "{events:?}");
    assert!(
        !events.iter().any(|action| action == "GiveUp"),
        "{events:?}"
    );
}

#[test]
fn a_call_whose_answer_is_refused_leaves_no_deadline_behind() {
    let clock = Arc::new(ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(9)));
    let store = SimEventStore::no_faults(9);
    // Finish's guard refuses a score under 50: the call ends, unanswered.
    let first = Process::start(
        &store,
        &clock,
        "lost-calls-a",
        &json!({ "params": { "score": 10 } }),
    );
    first.runtime.block_on(async {
        ask(&first.state).await;
        let mut ended = false;
        for _ in 0..5_000 {
            if store
                .list_scheduled_actions(TENANT)
                .await
                .unwrap()
                .is_empty()
            {
                ended = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(1)); // determinism-ok: real WASM blocking worker needs CPU; virtual time must stay paused
            tokio::task::yield_now().await;
        }
        assert!(ended, "the refused answer removes the call's deadline");
    });
    first.kill(&clock, Duration::from_secs(10));

    let status = status_after_restart(&store, &clock);
    let events = first_journal(&store);
    assert_eq!(status, "Asked", "{events:?}");
    assert!(
        !events.iter().any(|action| action == "GiveUp"),
        "{events:?}"
    );
}

/// The record's journal, read outside any process.
fn first_journal(store: &SimEventStore) -> Vec<String> {
    block(journal(store))
}

/// Run `future` to completion on a runtime of its own.
fn block<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(future)
}
