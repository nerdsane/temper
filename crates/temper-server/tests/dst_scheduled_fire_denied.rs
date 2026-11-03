//! DST: a scheduled action whose fire can never land is tried once, not
//! retried forever.
//!
//! A scheduled action fires as whoever armed it. When that caller cannot
//! make the move at all (here the fired action binds a parameter from the
//! caller's identity, and the caller has none), every try gets the same
//! denial. Such a fire is final, like a refusal: its row is removed and it is
//! not tried again. A fire the store failed is still retried on its row until
//! it lands.
//!
//! Workload, per seed: records arm `schedule('Take', 600)` or
//! `schedule('Fire', 600)` at seeded times. `Take` binds `owner_id` from the
//! authenticated subject. Each seed has at least one record of each case,
//! plus seeded extras:
//! - `NoContext`: armed by a caller with no security context;
//! - `ServiceCaller`: armed by a service, which has no caller identity (as a
//!   reaction's or a Wait wake's schedule is);
//! - `Person`: armed by a verified person, so `Take` binds their id and lands;
//! - `StoreFails(n)`: arms `Fire`, and the store fails the record's next `n`
//!   writes, so the fire's first tries fail and a later one lands.
//!
//! Invariants, checked long after every fire was due (several hours of
//! simulated time, well past many doubling retry waits and their one-hour
//! cap):
//! - a denied fire was dispatched exactly once, wrote nothing, left the
//!   record where it was, and left no row and no live timer behind;
//! - a person's fire was dispatched once and landed;
//! - a fire the store failed was dispatched once per failure and once more,
//!   landed exactly once, and left no row and no live timer behind.
//!
//! Dispatch tries are counted from the spans of the dispatch core, one per
//! try of the scheduled action's move. The server runs in a single-threaded,
//! paused-clock Tokio runtime, and `sim_now()` reads that clock.

#[path = "dst_state_timers_follow_state/process_clock.rs"]
mod process_clock;
#[path = "dst_state_timers_follow_state/world.rs"]
mod world;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use process_clock::ProcessClock;
use serde_json::json;
use temper_authz::{PrincipalKind, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::persistence::EventStore;
use temper_runtime::scheduler::{
    DeterministicIdGen, DeterministicRng, install_sim_context, sim_now,
};
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use tracing_subscriber::prelude::*;
use world::SimTimerRows;

const SEEDS: u64 = 16;
const TENANT: &str = "alpha";
/// How long after the last arm the run goes on. A fire retried with doubling
/// waits from one second, capped at an hour, would be tried about fifteen
/// times in this long.
const HORIZON: Duration = Duration::from_secs(4 * 3600);

const JOB: &str = r#"
[automaton]
name = "Job"
states = ["Idle", "Held", "Taken", "Waiting", "Done"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Held", "Taken", "Waiting", "Done"]
# A bound parameter needs strict parameters.
strict_action_params = true

[[state]]
name = "owner_id"
type = "string"
initial = ""

[[action]]
name = "Hold"
from = ["Idle"]
to = "Held"
effect = ["schedule('Take', 600)"]

[[action]]
name = "Take"
from = ["Held"]
to = "Taken"
params = [{ name = "owner_id", type = "string", source = "authenticated_subject" }]

[[action]]
name = "Start"
from = ["Idle"]
to = "Waiting"
effect = ["schedule('Fire', 600)"]

[[action]]
name = "Fire"
from = ["Waiting"]
to = "Done"
"#;

const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Jobs" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Job"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
<Property Name="owner_id" Type="Edm.String"/>
</EntityType>
<Action Name="Hold" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Take" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Start" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Fire" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<EntityContainer Name="Container"><EntitySet Name="Jobs" EntityType="Jobs.Job"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

/// Who arms a record's scheduled action, and what the store does to its fire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Case {
    /// A caller with no security context arms `Take`: denied.
    NoContext,
    /// A service arms `Take`: denied, it has no caller identity.
    ServiceCaller,
    /// A verified person arms `Take`: it lands.
    Person,
    /// `Fire`, whose first `n` writes the store fails.
    StoreFails(usize),
}

impl Case {
    /// The move that arms the scheduled action, and the action it fires.
    fn moves(self) -> (&'static str, &'static str) {
        match self {
            Self::NoContext | Self::ServiceCaller | Self::Person => ("Hold", "Take"),
            Self::StoreFails(_) => ("Start", "Fire"),
        }
    }

    /// The caller that arms it.
    fn caller(self) -> AgentContext {
        match self {
            Self::NoContext | Self::StoreFails(_) => AgentContext::default(),
            Self::ServiceCaller => AgentContext::for_service("wait-wake"),
            Self::Person => AgentContext {
                security_ctx: Some(SecurityContext::from_verified_jwt(
                    "person-principal",
                    PrincipalKind::Agent,
                    Some("test"),
                    Some("alice"),
                    None,
                    None,
                )),
                ..Default::default()
            },
        }
    }
}

/// Tries of a scheduled action's move, by record: one span of the dispatch
/// core per try of `Take` or `Fire`.
#[derive(Clone, Default)]
struct Tries(Arc<Mutex<BTreeMap<String, usize>>>);

impl Tries {
    fn of(&self, id: &str) -> usize {
        self.0.lock().unwrap().get(id).copied().unwrap_or(0)
    }

    fn clear(&self) {
        self.0.lock().unwrap().clear();
    }
}

/// The record a span names, kept on the span for its descendants to read.
struct NamesRecord(String);

/// The fields of a new span that say which record and which action it is.
#[derive(Default)]
struct SpanFields {
    entity_id: String,
    action: String,
}

impl SpanFields {
    fn set(&mut self, name: &str, value: String) {
        match name {
            "entity_id" => self.entity_id = value,
            "action_name" => self.action = value,
            _ => {}
        }
    }
}

impl tracing::field::Visit for SpanFields {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.set(field.name(), value.to_string());
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.set(
            field.name(),
            format!("{value:?}").trim_matches('"').to_string(),
        );
    }
}

/// The dispatch spans leave their record's id empty, so a try is counted for
/// the record its scheduled fire's span (`dispatch.scheduled_actions`, an
/// ancestor of every try of that fire) names.
impl<S> tracing_subscriber::Layer<S> for Tries
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let Some(span) = ctx.span(id) else {
            return;
        };
        let mut fields = SpanFields::default();
        attrs.record(&mut fields);
        if !fields.entity_id.is_empty() {
            span.extensions_mut()
                .insert(NamesRecord(fields.entity_id.clone()));
        }
        let scheduled_move = fields.action == "Take" || fields.action == "Fire";
        if attrs.metadata().name() != "dispatch_tenant_action_core" || !scheduled_move {
            return;
        }
        let record = span.scope().skip(1).find_map(|ancestor| {
            ancestor
                .extensions()
                .get::<NamesRecord>()
                .map(|named| named.0.clone())
        });
        if let Some(record) = record {
            *self.0.lock().unwrap().entry(record).or_default() += 1;
        }
    }
}

/// Let every task that can run, run.
async fn settle() {
    for _ in 0..600 {
        tokio::task::yield_now().await;
    }
}

/// Let `by` pass. The paused clock jumps from one timer to the next, so each
/// fires at its own instant.
async fn pass(by: Duration) {
    tokio::time::sleep(by).await;
    settle().await;
}

fn fail(seed: u64, trace: &[String], invariant: &str, detail: String) -> String {
    format!(
        "seed {seed}: {invariant}: {detail}\ntrace:\n  {}",
        trace.join("\n  ")
    )
}

/// One seed's run.
fn run_seed(seed: u64, tries: &Tries) -> Result<(), String> {
    let clock = Arc::new(ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let events = SimEventStore::no_faults(seed);
    let rows = Arc::new(SimTimerRows::new(seed));
    let mut rng = DeterministicRng::new(seed ^ 0xd3_71ed);
    let mut trace = Vec::new();
    tries.clear();

    // Every case at least once, then seeded extras, in a seeded order.
    let mut cases = vec![
        Case::NoContext,
        Case::ServiceCaller,
        Case::Person,
        Case::StoreFails(1 + rng.next_bound(3)),
    ];
    for _ in 0..rng.next_bound(4) {
        cases.push(match rng.next_bound(4) {
            0 => Case::NoContext,
            1 => Case::ServiceCaller,
            2 => Case::Person,
            _ => Case::StoreFails(1 + rng.next_bound(3)),
        });
    }
    for i in (1..cases.len()).rev() {
        cases.swap(i, rng.next_bound(i + 1));
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    let result = runtime.block_on(async {
        clock.begin();
        let mut registry = SpecRegistry::new();
        registry.register_tenant(
            TENANT,
            parse_csdl(CSDL).unwrap(),
            CSDL.to_string(),
            &[("Job", JOB)],
        );
        let mut state =
            ServerState::from_registry(ActorSystem::new(format!("denied-{seed}")), registry);
        state.set_storage_stack(StorageStack::from_sim(events.clone(), None));
        state.state_timer_rows = Some(rows.clone());
        let walk = state.start_runtime().await.expect("the server starts");
        walk.await.unwrap();
        let tenant = TenantId::new(TENANT);

        let mut records: Vec<(String, Case)> = Vec::new();
        for (i, case) in cases.iter().copied().enumerate() {
            let id = format!("job-{i}");
            pass(Duration::from_secs(rng.next_bound(300) as u64)).await;
            state
                .get_or_create_tenant_entity(&tenant, "Job", &id, json!({}))
                .await
                .map_err(|error| fail(seed, &trace, "records are created", error.to_string()))?;
            let (arm, _) = case.moves();
            let armed = state
                .dispatch_tenant_action(&tenant, "Job", &id, arm, json!({}), &case.caller())
                .await
                .is_ok_and(|response| response.success);
            if !armed {
                return Err(fail(seed, &trace, "records arm their actions", id));
            }
            if let Case::StoreFails(failures) = case {
                events.fail_next_appends(&format!("{TENANT}:Job:{id}"), failures);
            }
            trace.push(format!("{id}: {case:?}, {arm} at {}", sim_now()));
            records.push((id, case));
        }
        pass(HORIZON).await;
        settle().await;

        let pending = events.list_scheduled_actions(TENANT).await.unwrap();
        let armed = state.armed_scheduled_actions();
        for (id, case) in &records {
            let (_, fired) = case.moves();
            let journal = events
                .read_events(&format!("{TENANT}:Job:{id}"), 0)
                .await
                .map_err(|error| fail(seed, &trace, "journals are readable", error.to_string()))?;
            let writes = journal
                .iter()
                .filter(|event| event.event_type == fired)
                .count();
            let status = state
                .get_tenant_entity_state(&tenant, "Job", id)
                .await
                .map(|response| response.state.status)
                .map_err(|error| fail(seed, &trace, "records are readable", error.to_string()))?;
            let row_left = pending.iter().any(|row| &row.entity_id == id);
            let timer_left = armed.keys().any(|fire| &fire.entity_id == id);
            let tried = tries.of(id);
            let detail = format!(
                "{id} {case:?}: {fired} dispatched {tried} times, written {writes} times, \
                 status {status}, row left {row_left}, timer left {timer_left}"
            );
            if rows.row(TENANT, id).is_some() {
                return Err(fail(
                    seed,
                    &trace,
                    "a scheduled action keeps no state timer row",
                    detail,
                ));
            }
            if row_left || timer_left {
                return Err(fail(
                    seed,
                    &trace,
                    "a fired scheduled action leaves no row and no live timer",
                    detail,
                ));
            }
            // How many tries each case takes, how often its action is
            // written, and where its record ends.
            let (invariant, tries_ok, want_writes, want_status) = match *case {
                Case::NoContext | Case::ServiceCaller => (
                    "a fire that can never land is dispatched once and writes nothing",
                    tried == 1,
                    0,
                    "Held",
                ),
                Case::Person => (
                    "a person's fire lands on its first try",
                    tried == 1,
                    1,
                    "Taken",
                ),
                // The actor may meet more than one failure in a try, so at
                // most one try per failure and one more, and at least one retry.
                Case::StoreFails(failures) => (
                    "a fire the store failed is retried until it lands once",
                    (2..=failures + 1).contains(&tried),
                    1,
                    "Done",
                ),
            };
            if !tries_ok || writes != want_writes || status != want_status {
                return Err(fail(seed, &trace, invariant, detail));
            }
        }
        Ok(())
    });
    runtime.block_on(async { clock.end(Duration::ZERO) });
    drop(runtime);
    result
}

#[test]
fn dst_a_scheduled_fire_that_can_never_land_is_not_retried() {
    let tries = Tries::default();
    let _logs =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(tries.clone()));
    // Seeds that broke each invariant, and the first such seed's report.
    let mut broken: BTreeMap<String, (Vec<u64>, String)> = BTreeMap::new();
    for seed in 0..SEEDS {
        if let Err(report) = run_seed(seed, &tries) {
            let invariant = report.split(": ").nth(1).unwrap_or("unknown").to_string();
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
