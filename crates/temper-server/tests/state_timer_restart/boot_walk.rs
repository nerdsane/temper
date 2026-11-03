//! The boot walk replays only the records whose timer it could change: on a
//! store whose catalog follows its journal, a record whose row the boot armed
//! for the state it is in is left as it is, and a record in a timed state with
//! no row is still replayed and armed.
use super::*;

/// Ten records wait on timers with rows, and one repeating timer lost its row
/// (its process stopped between its event and its row). The next boot replays
/// only that one record, and arms it: it ticks.
fn boot_replays_only_records_it_could_change(world: &World) {
    let ids: Vec<String> = (0..10).map(|i| format!("waiting-{i}")).collect();
    let first = Server::start(world, "walk-run-1");
    let t0 = first.now();
    first.arm_all("Probe", &ids, &person("alice"));
    first.act("Ticker", "rowless", "Start", &person("alice"));
    first.forget_row("rowless");
    assert!(!first.has_row("rowless"), "the ticker has no row");
    let down = millis_between(&mut world.rng(), 0, 1_000);
    first.kill(world, t0 + Duration::from_millis(500) + down);

    let second = Server::start(world, "walk-run-2");
    let counts = second.boot_counts();
    let replayed = |entity_type: &str| {
        counts
            .iter()
            .find(|((tenant, counted), _)| tenant.as_str() == "default" && counted == entity_type)
            .map_or(0, |(_, counts)| counts.replayed)
    };
    assert_eq!(
        replayed("Probe"),
        0,
        "every record whose row follows its state is left as it is: {counts:?}"
    );
    assert_eq!(
        replayed("Ticker"),
        1,
        "the record in a timed state with no row is replayed: {counts:?}"
    );
    second.run_for(PERIOD + Duration::from_millis(500));
    assert!(
        second.read("Ticker", "rowless", "ticks").0 >= 1,
        "the boot armed the rowless ticker"
    );
    second.run_for(BUDGET);
    for id in &ids {
        assert_eq!(
            second.read("Probe", id, "fires"),
            (1, json!("alice")),
            "{id} fires once, as whoever armed it"
        );
    }
    let stopped = second.now();
    second.kill(world, stopped);
}

#[test]
fn a_boot_replays_only_the_records_whose_timer_it_could_change() {
    on_turso(boot_replays_only_records_it_could_change);
}
