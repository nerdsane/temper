//! A timer's move that changes nothing writes nothing, and its timer keeps
//! its pace (`entity_actor::unrecorded`).
//!
//! The poller's `Poll` repeats every [`POLL`] and changes nothing; its trigger
//! is a relay's `Pass`, a reaction's move that changes nothing either, whose
//! own trigger counts on a tally. So every period the tally's journal gains a
//! `Count`, while the poller's and the relay's gain nothing, and the poller's
//! timer row stays as it is. A person's `Poll` changes nothing too, and is
//! written once, as the first to name them; asked again, it writes nothing. A
//! restart finds the row past due and looks once at once, then keeps the pace.

use std::time::Duration;

use temper_runtime::persistence::StateTimerRecord;

use super::server::{SETTLE, Server, World, millis_between, person};

/// `POLLER_IOA`'s and `COUNTING_POLLER_IOA`'s `after_seconds`.
const POLL: Duration = Duration::from_secs(15);

/// How far from a poll's deadline a check looks: before it, nothing new has
/// fired; after it, the poll and everything it set off have.
const MARGIN: Duration = Duration::from_millis(500);

/// How many times `action` appears in `journal`.
fn count(journal: &[String], action: &str) -> usize {
    journal.iter().filter(|entry| *entry == action).count()
}

/// The ids of `rows`, which say which arm each timer row belongs to.
fn arm_ids(rows: &[StateTimerRecord]) -> Vec<String> {
    rows.iter().map(|row| row.arm_id.clone()).collect()
}

/// A poller watching since now, with its relay and tally made.
fn watch(server: &Server, id: &str) {
    server.create("Relay", id);
    server.create("Tally", id);
    server.act("Poller", id, "Watch", &person("alice"));
}

pub(crate) fn polls_that_change_nothing_write_nothing_and_still_trigger(world: &World) {
    let id = "quiet-poller";
    let server = Server::start(world, "unrecorded-run-1");
    let t0 = server.now();
    watch(&server, id);
    let watched = server.journal("Poller", id);
    let relayed = server.journal("Relay", id);
    let row = server.rows();
    assert_eq!(
        count(&watched, "Watch"),
        1,
        "the person's Watch is recorded"
    );

    for period in 1..=4u32 {
        server.run_until(t0 + POLL * period - MARGIN);
        assert_eq!(
            server.read("Tally", id, "count").0,
            period as usize - 1,
            "a poll that wrote no event re-arms from its own time, not the last \
             event's: it does not fire before its period"
        );
        server.run_until(t0 + POLL * period + MARGIN);
        assert_eq!(
            server.read("Tally", id, "count").0,
            period as usize,
            "each period's poll still sets off its trigger, and its trigger's"
        );
    }
    assert_eq!(
        server.journal("Poller", id),
        watched,
        "a timer's poll that changes nothing writes no event"
    );
    assert_eq!(
        server.journal("Relay", id),
        relayed,
        "a reaction's move that changes nothing writes no event"
    );
    assert_eq!(
        arm_ids(&server.rows()),
        arm_ids(&row),
        "a poll that changes nothing writes no timer row"
    );
    assert_eq!(
        count(&server.journal("Tally", id), "Count"),
        4,
        "the trigger's move that changes its record is written once per period"
    );

    // The same Poll, asked for by a person. The first names someone new, so it
    // is written: the record says who moved it last, and its timers follow them.
    server.act("Poller", id, "Poll", &person("bob"));
    let asked = server.journal("Poller", id);
    assert_eq!(
        count(&asked, "Poll"),
        1,
        "the first move to name someone is written: {asked:?}"
    );
    assert_eq!(server.read("Tally", id, "count").0, 5);
    assert_ne!(
        arm_ids(&server.rows()),
        arm_ids(&row),
        "a reset by someone else writes the timer's row"
    );

    // The same person again: nothing new to say, so no event, and its trigger
    // still runs.
    server.act("Poller", id, "Poll", &person("bob"));
    let again = server.journal("Poller", id);
    assert_eq!(
        count(&again, "Poll"),
        1,
        "the same caller's move that changes nothing writes no event: {again:?}"
    );
    assert_eq!(server.read("Tally", id, "count").0, 6);
    let stopped = server.now();
    server.kill(world, stopped);
}

pub(crate) fn polls_that_change_the_record_are_recorded(world: &World) {
    let id = "counting-poller";
    let server = Server::start(world, "unrecorded-run-2");
    let t0 = server.now();
    server.act("CountingPoller", id, "Watch", &person("alice"));
    server.run_until(t0 + POLL * 3 + MARGIN);
    assert_eq!(server.read("CountingPoller", id, "polls").0, 3);
    assert_eq!(
        count(&server.journal("CountingPoller", id), "Poll"),
        3,
        "a timer's poll that changes its record is written, one per period"
    );
    let stopped = server.now();
    server.kill(world, stopped);
}

/// A poll that wrote nothing leaves its row's clock behind the live timer's. A
/// reconcile reads that row, and a move in `fire_now_on` newer than the row but
/// older than the live clock must not bring the timer forward a second time.
pub(crate) fn an_unrelated_move_does_not_fire_a_poll_early(world: &World) {
    let id = "noted-poller";
    let server = Server::start(world, "unrecorded-run-3");
    let t0 = server.now();
    watch(&server, id);
    server.run_until(t0 + POLL + MARGIN);
    assert_eq!(server.read("Tally", id, "count").0, 1);

    server.act("Poller", id, "Heard", &person("carol"));
    server.run_for(SETTLE);
    assert_eq!(
        server.read("Tally", id, "count").0,
        2,
        "the delivery brings the poll forward once"
    );
    let looked = server.now();

    server.act("Poller", id, "Note", &person("carol"));
    server.run_for(SETTLE);
    assert_eq!(
        server.read("Tally", id, "count").0,
        2,
        "a move that does not reset the timer leaves it waiting"
    );
    server.run_until(looked + POLL - MARGIN);
    assert_eq!(
        server.read("Tally", id, "count").0,
        2,
        "the poll waits its period from the look the delivery brought forward"
    );
    server.run_until(looked + POLL + MARGIN);
    assert_eq!(server.read("Tally", id, "count").0, 3);
    let stopped = server.now();
    server.kill(world, stopped);
}

pub(crate) fn a_restart_looks_once_then_keeps_the_pace(world: &World) {
    let mut rng = world.rng();
    // Down from just after the third poll until some time before the fourth
    // falls due. The polls wrote no row, so the row is three periods old.
    let back = millis_between(&mut rng, 1_500, 13_000);
    let id = "restarted-poller";

    let first = Server::start(world, "unrecorded-restart-1");
    let t0 = first.now();
    watch(&first, id);
    let watched = first.journal("Poller", id);
    first.run_until(t0 + POLL * 3 + MARGIN);
    assert_eq!(first.read("Tally", id, "count").0, 3);
    assert!(first.has_row(id), "the poll's timer keeps its row");
    let back_up = t0 + POLL * 3 + back;
    first.kill(world, back_up);

    let second = Server::start(world, "unrecorded-restart-2");
    second.run_for(SETTLE);
    assert_eq!(
        second.read("Tally", id, "count").0,
        4,
        "the restart finds the row past due, and the poll looks once at once"
    );
    second.run_until(back_up + POLL - MARGIN);
    assert_eq!(
        second.read("Tally", id, "count").0,
        4,
        "that look re-armed the poll: it does not fire again early"
    );
    second.run_until(back_up + POLL + SETTLE + MARGIN);
    assert_eq!(
        second.read("Tally", id, "count").0,
        5,
        "the poll fires a period after that look"
    );
    assert_eq!(
        second.journal("Poller", id),
        watched,
        "no poll wrote an event, before or after the restart"
    );
    let stopped = second.now();
    second.kill(world, stopped);
}

/// A reset after a one-shot fire is a new arm even though its move has no
/// journal entry. Unrelated recorded moves and boot reconciliation must use
/// that arm's clock, including after the actor has replayed its old fire.
pub(crate) fn one_shot_reset_survives_reconciliation_and_restart(world: &World) {
    use temper_server::entity_actor::{EventOrigin, EventVia};

    for note_before_restart in [false, true] {
        let id = if note_before_restart {
            "reset-noted"
        } else {
            "reset-only"
        };
        let mut rng = world.rng();
        let first = Server::start(world, "one-shot-reset-1");
        let t0 = first.now();
        first.act("Probe", id, "Arm", &person("alice"));
        first.run_until(t0 + super::BUDGET + MARGIN);
        assert_eq!(first.read("Probe", id, "fires").0, 1);
        assert!(!first.has_row(id), "the original one-shot finished");
        let journal = first.journal("Probe", id);
        let reset_at = first.now();
        let mut reaction = person("alice");
        reaction.event_origin = Some(EventOrigin {
            via: Some(EventVia::Reaction),
            ..Default::default()
        });
        first.act("Probe", id, "Reset", &reaction);
        assert_eq!(
            first.journal("Probe", id),
            journal,
            "the reset writes no event"
        );
        assert!(first.has_row(id), "the reset has its own durable arm");
        if note_before_restart {
            first.act("Probe", id, "Note", &person("alice"));
            assert!(
                first.has_row(id),
                "an unrelated event preserves the reset arm"
            );
        }
        first.kill(world, reset_at + millis_between(&mut rng, 500, 1_500));

        let second = Server::start(world, "one-shot-reset-2");
        second.run_for(SETTLE);
        assert!(
            second.has_row(id),
            "boot reconciliation preserves the newer reset"
        );
        second.act("Probe", id, "Note", &person("bob"));
        assert!(
            second.has_row(id),
            "a replayed actor's unrelated event preserves the reset"
        );
        second.run_until(reset_at + super::BUDGET - MARGIN);
        assert_eq!(second.read("Probe", id, "fires").0, 1, "no early fire");
        second.run_until(reset_at + super::BUDGET + MARGIN);
        assert_eq!(
            second.read("Probe", id, "fires").0,
            2,
            "the reset one-shot fires"
        );
        assert!(!second.has_row(id), "the new one-shot ends its row");
        second.run_for(super::BUDGET * 2);
        assert_eq!(
            second.read("Probe", id, "fires").0,
            2,
            "it fires only once per reset"
        );
        assert!(
            !second
                .journal("Probe", id)
                .iter()
                .any(|action| action == "Reset")
        );
        let stopped = second.now();
        second.kill(world, stopped);
    }
}
