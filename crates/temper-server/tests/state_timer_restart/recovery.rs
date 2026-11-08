//! Recovery cases of the restart tests: timers with no row, timers that
//! already fired, many overdue timers at boot, and a fire whose row survived.
use super::*;
use temper_server::request_context::AgentContext;

/// A timer with no row (armed before rows were kept) comes back on its
/// record's next action and counts the time already spent in its state, even
/// when the entry into the state is no longer among the retained events (the
/// window keeps 50; a snapshot keeps none). Before the status clock it was
/// armed for a full budget on every such hydration, so a daily timer on a
/// server restarted more often than daily never fired (Inspiration Bank's
/// feeds, 15 to 20 Oct 2026).
fn timer_without_a_row_counts_time_in_state(world: &World) {
    let mut rng = world.rng();
    let notes = 55 + rng.next_bound(20);
    // Back up `late` past the timer's deadline.
    let late = millis_between(&mut rng, 100, 2_000);

    let first = Server::start(world, "clock-run-1");
    let t0 = first.now();
    first.act("Probe", "noted", "Arm", &person("alice"));
    for _ in 0..notes {
        first.act("Probe", "noted", "Note", &person("alice"));
    }
    first.forget_rows();
    assert_eq!(
        first.read("Probe", "noted", "fires").0,
        0,
        "armed and waiting"
    );
    first.kill(world, t0 + BUDGET + late);

    let second = Server::start(world, "clock-run-2");
    second.run_for(SETTLE);
    assert_eq!(
        second.read("Probe", "noted", "fires").0,
        0,
        "no row: nothing armed at boot"
    );
    second.act("Probe", "noted", "Note", &person("bob"));
    second.run_for(SETTLE);
    assert_eq!(
        second.read("Probe", "noted", "fires").0,
        1,
        "overdue: the next action's re-arm fires it at once"
    );
    let stopped = second.now();
    second.kill(world, stopped);
}

#[test]
fn a_timer_without_a_row_counts_the_time_already_spent_in_its_state() {
    on_turso(timer_without_a_row_counts_time_in_state);
}

/// A timer whose action keeps the record in its state (and does not reset the
/// clock) fires once: after a restart, the record's next action finds it
/// already fired and does not arm it again.
fn fired_timer_does_not_fire_again(world: &World) {
    let down = millis_between(&mut world.rng(), 0, 10_000);
    let first = Server::start(world, "fired-run-1");
    let t0 = first.now();
    first.act("Probe", "fired", "Arm", &person("alice"));
    first.run_until(t0 + BUDGET + Duration::from_millis(500));
    assert_eq!(first.read("Probe", "fired", "fires").0, 1);
    let stopped = first.now();
    first.kill(world, stopped + down);

    let second = Server::start(world, "fired-run-2");
    second.act("Probe", "fired", "Note", &person("bob"));
    // Past a full budget: a timer armed again would have fired.
    second.run_for(BUDGET + Duration::from_millis(500));
    assert_eq!(
        second.read("Probe", "fired", "fires"),
        (1, json!("alice")),
        "it fired once, before the restart"
    );
    let stopped = second.now();
    second.kill(world, stopped);
}

#[test]
fn a_timer_that_fired_does_not_fire_again_after_a_restart() {
    on_turso(fired_timer_does_not_fire_again);
}

/// Many timers that fell due while the server was down: at boot every one
/// fires exactly once, as whoever armed it (the boot gate lets a few fire at a
/// time; this checks it loses and repeats none).
fn many_overdue_timers_fire_once(world: &World) {
    let late = millis_between(&mut world.rng(), 100, 2_000);
    let ids: Vec<String> = (0..40).map(|i| format!("overdue-{i}")).collect();
    let first = Server::start(world, "burst-run-1");
    let t0 = first.now();
    first.arm_all("Probe", &ids, &person("alice"));
    let waiting = ids
        .iter()
        .filter(|id| first.read("Probe", id, "fires").0 == 0)
        .count();
    assert_eq!(waiting, ids.len(), "every timer waits at the restart");
    first.kill(world, t0 + BUDGET + late);

    let second = Server::start(world, "burst-run-2");
    second.run_for(Duration::from_secs(10));
    for id in &ids {
        assert_eq!(
            second.read("Probe", id, "fires").0,
            1,
            "{id} fires after the restart"
        );
    }
    second.run_for(Duration::from_secs(1));
    for id in &ids {
        assert_eq!(
            second.read("Probe", id, "fires"),
            (1, json!("alice")),
            "{id}"
        );
    }
    let stopped = second.now();
    second.kill(world, stopped);
}

#[test]
fn many_overdue_timers_each_fire_once_after_restart() {
    on_turso(many_overdue_timers_fire_once);
}

/// The fire is a new action: the arming request's idempotency key, which the
/// entity has already recorded, must not make it a duplicate that never runs.
fn keyed_arm_fires(world: &World) {
    let server = Server::start(world, "keyed-arm");
    let t0 = server.now();
    let keyed = AgentContext {
        idempotency_key: Some(temper_server::idempotency::IdempotencyKey::Request(
            "request-1".to_string(),
        )),
        ..person("alice")
    };
    server.act("Probe", "keyed", "Arm", &keyed);
    server.run_until(t0 + BUDGET + Duration::from_millis(500));
    assert_eq!(server.read("Probe", "keyed", "fires"), (1, json!("alice")));
    let stopped = server.now();
    server.kill(world, stopped);
}

#[test]
fn timer_armed_by_a_request_with_an_idempotency_key_fires() {
    on_turso(keyed_arm_fires);
}

/// A fire that committed before a crash left its row behind (its delete never
/// happened): the next boot arms that row again, finds the action already ran
/// since the clock started, and ends the timer without firing.
fn surviving_row_runs_once(world: &World) {
    let down = millis_between(&mut world.rng(), 0, 10_000);
    let first = Server::start(world, "kept-row-1");
    let t0 = first.now();
    first.act("Probe", "kept", "Arm", &person("alice"));
    let row = first
        .rows()
        .into_iter()
        .find(|row| row.entity_id == "kept")
        .expect("the arm's row");
    first.run_until(t0 + BUDGET + Duration::from_millis(500));
    assert_eq!(first.read("Probe", "kept", "fires").0, 1);
    assert!(!first.has_row("kept"), "the fired timer's row stays");
    first.restore_row(&row);
    let stopped = first.now();
    first.kill(world, stopped + down);

    let second = Server::start(world, "kept-row-2");
    second.run_for(Duration::from_secs(1));
    assert!(!second.has_row("kept"), "the boot did not take the row");
    assert_eq!(
        second.read("Probe", "kept", "fires"),
        (1, json!("alice")),
        "the boot took the row and ended it; the action ran once"
    );
    let stopped = second.now();
    second.kill(world, stopped);
}

#[test]
fn a_fire_whose_row_survived_runs_once_after_a_restart() {
    on_turso(surviving_row_runs_once);
}
