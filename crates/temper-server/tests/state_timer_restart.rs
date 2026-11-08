//! Armed state timers survive a server restart (ADR-0056).
//!
//! After the restart, a timer that fell due while the server was down fires
//! at once, one still waiting fires when its budget ends (not a full budget
//! after boot), each fires exactly once and as the principal whose action
//! armed it, and a repeating timer does not replay the ticks it missed. Run
//! against Turso always, and against PostgreSQL when
//! `TEMPER_TEST_POSTGRES_URL` names a disposable database.
//!
//! Time is simulated (see `state_timer_restart/server.rs`): every server
//! runs on a paused clock that moves only while the scenario waits and no
//! task can run, and the store is real. Each scenario runs [`SEEDS`] seeds;
//! a seed draws the scenario's times within the bounds each one states, and
//! its ids.

#[path = "state_timer_restart/boot_walk.rs"]
mod boot_walk;
#[path = "dst_state_timers_follow_state/process_clock.rs"]
mod process_clock;
#[path = "state_timer_restart/recovery.rs"]
mod recovery;
#[path = "state_timer_restart/server.rs"]
mod server;
#[path = "state_timer_restart/unrecorded.rs"]
mod unrecorded;

use std::time::Duration;

use serde_json::json;
use server::{Backend, SETTLE, Server, World, millis_between, person, turso};
use temper_store_postgres::{PostgresSchema, migration};

/// Seeds per scenario.
const SEEDS: u64 = 8;

/// `PROBE_IOA`'s `after_seconds`.
const BUDGET: Duration = Duration::from_secs(4);

/// `TICKER_IOA`'s `after_seconds`.
const PERIOD: Duration = Duration::from_secs(2);

/// `Fire` records who it ran as and counts its runs; it stays in `Armed`
/// and does not reset the timer, so the timer fires once.
const PROBE_IOA: &str = r#"
[automaton]
name = "Probe"
states = ["Idle", "Armed"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Armed"]
strict_action_params = true

[[state]]
name = "fired_by"
type = "string"
initial = ""

[[state]]
name = "fires"
type = "counter"
initial = 0

[[action]]
name = "Arm"
from = ["Idle"]
to = "Armed"
params = []

[[action]]
name = "Note"
from = ["Armed"]
params = []

[[action]]
name = "Reset"
from = ["Armed"]
params = []

[[action]]
name = "Fire"
from = ["Armed"]
params = [{ name = "fired_by", type = "string", source = "authenticated_subject" }]
effect = ["fires += 1"]

[[state_timeout]]
state = "Armed"
after_seconds = 4
on_timeout = "Fire"
reset_on = ["Reset"]
"#;

/// A repeating timer: every `Tick` resets the clock for the next one.
const TICKER_IOA: &str = r#"
[automaton]
name = "Ticker"
states = ["Idle", "Running"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Running"]
strict_action_params = true

[[state]]
name = "ticks"
type = "counter"
initial = 0

[[action]]
name = "Start"
from = ["Idle"]
to = "Running"
params = []

[[action]]
name = "Tick"
from = ["Running"]
params = []
effect = ["ticks += 1"]

[[state_timeout]]
state = "Running"
after_seconds = 2
on_timeout = "Tick"
reset_on = ["Tick"]
"#;

/// A watcher whose timer polls every 15 s. `Poll` changes nothing: it only
/// sets off its trigger, a `Pass` on the record's relay.
const POLLER_IOA: &str = r#"
[automaton]
name = "Poller"
states = ["Idle", "Watching"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Watching"]
strict_action_params = true

[[state]]
name = "notes"
type = "counter"
initial = 0

[[action]]
name = "Watch"
from = ["Idle"]
to = "Watching"
params = []

[[action]]
name = "Note"
from = ["Watching"]
params = []
effect = ["notes += 1"]

[[action]]
name = "Heard"
from = ["Watching"]
params = []

[[action]]
name = "Poll"
from = ["Watching"]
params = []

[[action.triggers]]
name = "relays"
kind = "entity"
target_entity = "Relay"
target_action = "Pass"
resolve_target = { kind = "same_id" }

[[state_timeout]]
state = "Watching"
after_seconds = 15
on_timeout = "Poll"
reset_on = ["Poll"]
fire_now_on = ["Heard"]
"#;

/// `POLLER_IOA` whose `Poll` counts itself, so each poll changes the record.
const COUNTING_POLLER_IOA: &str = r#"
[automaton]
name = "CountingPoller"
states = ["Idle", "Watching"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Watching"]
strict_action_params = true

[[state]]
name = "polls"
type = "counter"
initial = 0

[[action]]
name = "Watch"
from = ["Idle"]
to = "Watching"
params = []

[[action]]
name = "Poll"
from = ["Watching"]
params = []
effect = ["polls += 1"]

[[state_timeout]]
state = "Watching"
after_seconds = 15
on_timeout = "Poll"
reset_on = ["Poll"]
"#;

/// A reaction's hop that changes nothing: `Pass` only sets off its own
/// trigger, a `Count` on the record's tally.
const RELAY_IOA: &str = r#"
[automaton]
name = "Relay"
states = ["Open"]
initial = "Open"
allow_indefinite_states = ["Open"]
strict_action_params = true

[[action]]
name = "Pass"
from = ["Open"]
params = []

[[action.triggers]]
name = "tallies"
kind = "entity"
target_entity = "Tally"
target_action = "Count"
resolve_target = { kind = "same_id" }
"#;

/// Counts what reaches it, so each `Count` changes the record.
const TALLY_IOA: &str = r#"
[automaton]
name = "Tally"
states = ["Open"]
initial = "Open"
allow_indefinite_states = ["Open"]
strict_action_params = true

[[state]]
name = "count"
type = "counter"
initial = 0

[[action]]
name = "Count"
from = ["Open"]
params = []
effect = ["count += 1"]
"#;

const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Probe" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Probe"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
<Property Name="fired_by" Type="Edm.String"/>
</EntityType>
<EntityType Name="Ticker"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
</EntityType>
<EntityType Name="Poller"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
</EntityType>
<EntityType Name="CountingPoller"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
</EntityType>
<EntityType Name="Relay"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
</EntityType>
<EntityType Name="Tally"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
</EntityType>
<Action Name="Arm" IsBound="true"><Parameter Name="bindingParameter" Type="Probe.Probe"/></Action>
<Action Name="Fire" IsBound="true"><Parameter Name="bindingParameter" Type="Probe.Probe"/></Action>
<Action Name="Note" IsBound="true"><Parameter Name="bindingParameter" Type="Probe.Probe"/></Action>
<Action Name="Start" IsBound="true"><Parameter Name="bindingParameter" Type="Probe.Ticker"/></Action>
<Action Name="Tick" IsBound="true"><Parameter Name="bindingParameter" Type="Probe.Ticker"/></Action>
<EntityContainer Name="Container">
<EntitySet Name="Probes" EntityType="Probe.Probe"/>
<EntitySet Name="Tickers" EntityType="Probe.Ticker"/>
<EntitySet Name="Pollers" EntityType="Probe.Poller"/>
<EntitySet Name="CountingPollers" EntityType="Probe.CountingPoller"/>
<EntitySet Name="Relays" EntityType="Probe.Relay"/>
<EntitySet Name="Tallies" EntityType="Probe.Tally"/>
</EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

/// Test-only blanket permit: this file checks timers, not authorization.
const PERMIT_ALL: &str = "permit(principal, action, resource);";

/// Run `scenario` once per seed, each on a fresh Turso database.
fn on_turso(scenario: fn(&World)) {
    for seed in 0..SEEDS {
        let (_dir, backend) = turso();
        eprintln!("seed {seed}");
        scenario(&World::new(seed, backend));
    }
}

/// The overdue and the due-soon timer fire once each, on time, as whoever armed them.
fn timers_fire_once_as_their_principal_after_restart(world: &World) {
    let mut rng = world.rng();
    // `due-soon` is armed `gap` after `overdue`. The server is down from
    // then until `late` past `overdue`'s deadline, at least 0.8 s before
    // `due-soon`'s.
    let gap = millis_between(&mut rng, 1_500, 3_000);
    let late = millis_between(&mut rng, 200, gap.as_millis() as u64 - 800);

    let first = Server::start(world, "timers-run-1");
    let t0 = first.now();
    first.act("Probe", "overdue", "Arm", &person("alice"));
    first.run_until(t0 + gap);
    first.act("Probe", "due-soon", "Arm", &person("bob"));
    first.run_for(SETTLE);
    assert!(
        first.has_row("overdue") && first.has_row("due-soon"),
        "each waiting timer has its row: {:?}",
        first.rows()
    );
    first.kill(world, t0 + BUDGET + late);

    let second = Server::start(world, "timers-run-2");
    let booted = second.now();
    let due_soon = t0 + gap + BUDGET;
    second.run_for(SETTLE);
    assert_eq!(
        second.read("Probe", "overdue", "fires").0,
        1,
        "a timer that fell due while the server was down fires at once"
    );
    assert_eq!(
        second.read("Probe", "due-soon", "fires").0,
        0,
        "a waiting timer does not fire early"
    );
    second.run_until(due_soon - Duration::from_millis(300));
    assert_eq!(
        second.read("Probe", "due-soon", "fires").0,
        0,
        "a waiting timer does not fire early"
    );
    // A full budget after boot would end at `booted + BUDGET`, at least
    // 1.2 s after this.
    second.run_until(due_soon + Duration::from_millis(500));
    assert!(
        booted + BUDGET > second.now(),
        "this check comes before a full budget after boot"
    );
    assert_eq!(
        second.read("Probe", "due-soon", "fires").0,
        1,
        "a waiting timer fires when its own budget ends"
    );

    // Past the point where a second firing of either would show.
    second.run_until(due_soon + BUDGET + Duration::from_millis(500));
    assert_eq!(
        second.read("Probe", "overdue", "fires"),
        (1, json!("alice"))
    );
    assert_eq!(second.read("Probe", "due-soon", "fires"), (1, json!("bob")));
    let stopped = second.now();
    second.kill(world, stopped);

    // Both timers ended: a later restart has nothing to fire.
    let third = Server::start(world, "timers-run-3");
    third.run_for(BUDGET + Duration::from_millis(500));
    assert_eq!(third.read("Probe", "overdue", "fires").0, 1);
    assert_eq!(third.read("Probe", "due-soon", "fires").0, 1);
    let stopped = third.now();
    third.kill(world, stopped);
}

/// A repeating timer that missed several ticks fires once, then keeps its pace.
fn repeating_timer_resumes_without_replaying_missed_ticks(world: &World) {
    let mut rng = world.rng();
    // Stopped between its first and second tick, then down for more than
    // two more periods.
    let stop_after = millis_between(&mut rng, 2_200, 3_800);
    let down = millis_between(&mut rng, 5_000, 11_000);

    let first = Server::start(world, "ticker-run-1");
    let t0 = first.now();
    first.act("Ticker", "feed", "Start", &person("alice"));
    first.run_until(t0 + stop_after);
    assert_eq!(
        first.read("Ticker", "feed", "ticks").0,
        1,
        "the first tick fires a period after the start"
    );
    assert!(first.has_row("feed"), "the next tick has its row");
    first.kill(world, t0 + stop_after + down);

    let second = Server::start(world, "ticker-run-2");
    let booted = second.now();
    second.run_for(SETTLE);
    assert_eq!(
        second.read("Ticker", "feed", "ticks").0,
        2,
        "the overdue tick fires at once"
    );
    second.run_until(booted + Duration::from_millis(1_500));
    assert_eq!(
        second.read("Ticker", "feed", "ticks").0,
        2,
        "missed ticks are not replayed"
    );
    second.run_until(booted + PERIOD + Duration::from_millis(500));
    assert_eq!(
        second.read("Ticker", "feed", "ticks").0,
        3,
        "the next tick comes a full period after the catch-up"
    );
    second.run_until(booted + PERIOD * 3 + Duration::from_millis(500));
    assert_eq!(
        second.read("Ticker", "feed", "ticks").0,
        5,
        "and the ticks keep that pace"
    );
    let stopped = second.now();
    second.kill(world, stopped);
}

#[test]
fn turso_timers_fire_once_as_their_principal_after_restart() {
    on_turso(timers_fire_once_as_their_principal_after_restart);
}

#[test]
fn turso_repeating_timer_resumes_without_replaying_missed_ticks() {
    on_turso(repeating_timer_resumes_without_replaying_missed_ticks);
}

#[test]
fn turso_polls_that_change_nothing_write_nothing_and_still_trigger() {
    on_turso(unrecorded::polls_that_change_nothing_write_nothing_and_still_trigger);
}

#[test]
fn turso_polls_that_change_the_record_are_recorded() {
    on_turso(unrecorded::polls_that_change_the_record_are_recorded);
}

#[test]
fn turso_one_shot_reset_survives_reconciliation_and_restart() {
    on_turso(unrecorded::one_shot_reset_survives_reconciliation_and_restart);
}

#[test]
fn turso_an_unrelated_move_does_not_fire_a_poll_early() {
    on_turso(unrecorded::an_unrelated_move_does_not_fire_a_poll_early);
}

#[test]
fn turso_a_restart_looks_once_then_keeps_the_pace() {
    on_turso(unrecorded::a_restart_looks_once_then_keeps_the_pace);
}

#[test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
fn postgres_timers_survive_restart() {
    let url = std::env::var("TEMPER_TEST_POSTGRES_URL").expect("disposable PostgreSQL"); // determinism-ok: integration-test configuration
    let runtime = tokio::runtime::Runtime::new().unwrap();
    for seed in 0..SEEDS {
        // Named before the seed's context is installed, so every run has its own.
        let schema = PostgresSchema::new(format!(
            "state_timers_{}",
            temper_runtime::scheduler::sim_uuid().simple()
        ))
        .unwrap();
        runtime.block_on(async {
            let pool = sqlx::PgPool::connect(&url).await.unwrap();
            migration::run_migrations_in_schema(&pool, &schema)
                .await
                .unwrap();
        });
        eprintln!("seed {seed}");
        {
            let world = World::new(seed, Backend::Postgres(url.clone(), schema.clone()));
            timers_fire_once_as_their_principal_after_restart(&world);
            repeating_timer_resumes_without_replaying_missed_ticks(&world);
            unrecorded::a_restart_looks_once_then_keeps_the_pace(&world);
            unrecorded::one_shot_reset_survives_reconciliation_and_restart(&world);
        }
        runtime.block_on(async {
            let pool = sqlx::PgPool::connect(&url).await.unwrap();
            sqlx::query(&format!(
                "DROP SCHEMA {} CASCADE",
                schema.qualify_sql("{schema}").trim_end_matches('.')
            ))
            .execute(&pool)
            .await
            .unwrap();
        });
    }
}
