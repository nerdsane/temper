//! Seeded simulations of writes racing on one record.
//!
//! An external write is decided on the record as it was read (Cedar and the
//! write's other checks) and may land only on that state: the record's actor
//! refuses it when the record changed first (the authorization precondition).
//! Each seed runs several writes on one record at once, in one single-threaded
//! execution against the simulated journal. The cooperative yield points take
//! a seeded number of turns, so the writes reach the record in a different
//! order on each seed, and the same seed replays the same execution.
//!
//! The invariants:
//! - A write its policy allows on the state it lands on lands, however many
//!   writes race it: N allowed moves sent at once are each answered on their
//!   first attempt, and the count is N. None is answered as stale.
//! - No write lands on a state its policy refuses: a move allowed only while
//!   the record's project is Open never lands after the record was pointed at
//!   a Closed project, though it was decided while the project was Open. It is
//!   decided again on the record as it is, and refused by the policy.
//! - A person and the runs acting for them move one record at once: each move
//!   lands or is refused by its guard on the record as it is, never as stale.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::future::join_all;
use serde_json::{Value, json};
use temper_runtime::buggify::install_buggify_context;
use temper_runtime::persistence::EventStore as _;
use temper_runtime::scheduler::install_deterministic_context;

#[path = "dst_stale_writes/world.rs"]
mod world;

use world::*;

/// A move its guard refused on the record as it is, by a guard expression or
/// by the states it may start from: not stale.
fn refused_by_guard(status: StatusCode, body: &Value) -> bool {
    status == StatusCode::CONFLICT
        && matches!(
            body["error"]["code"].as_str(),
            Some("GuardFailed" | "InvalidState")
        )
}

/// N allowed moves race on one record: each lands on its first attempt.
async fn allowed_moves_race(seed: u64) -> Result<(), String> {
    let _sim = install_deterministic_context(seed);
    let state = server(seed, "moves");
    create(&state, "Projects", json!({ "Id": "p1" })).await?;
    create(&state, "Tallies", json!({ "Id": "t1", "project_id": "p1" })).await?;

    let _faults = install_buggify_context(seed);
    let who: Vec<String> = (0..RACING_MOVES).map(|i| format!("c{i}")).collect();
    let answers = join_all(who.iter().map(|id| {
        act(
            &state,
            id,
            "Tallies('t1')/Temper.Stale.Bump",
            json!({ "last": id }),
        )
    }))
    .await;

    let refused: Vec<String> = who
        .iter()
        .zip(&answers)
        .filter(|(_, (status, _))| *status != StatusCode::OK)
        .map(|(id, (status, body))| format!("{id}: {status} {body}"))
        .collect();
    if !refused.is_empty() {
        return Err(format!(
            "{} of {RACING_MOVES} allowed moves did not land on their first attempt: {}",
            refused.len(),
            refused.join("; ")
        ));
    }
    let record = tally(&state, "t1").await;
    let count = record.counters.get("count").copied().unwrap_or_default();
    if count != RACING_MOVES {
        return Err(format!(
            "{RACING_MOVES} moves landed but the count is {count}"
        ));
    }
    Ok(())
}

/// Mallory's moves, each allowed only while the tally's project is Open, race
/// the owner pointing the tally at a Closed project.
async fn linked_record_attack(seed: u64) -> Result<(), String> {
    let _sim = install_deterministic_context(seed);
    let state = server(seed, "attack");
    create(&state, "Projects", json!({ "Id": "p1" })).await?;
    create(&state, "Projects", json!({ "Id": "p2" })).await?;
    let (status, body) = act(
        &state,
        "owner",
        "Projects('p2')/Temper.Stale.Close",
        json!({}),
    )
    .await;
    if status != StatusCode::OK {
        return Err(format!("closing p2: {status} {body}"));
    }
    create(&state, "Tallies", json!({ "Id": "t1", "project_id": "p1" })).await?;

    let _faults = install_buggify_context(seed);
    let attempts: Vec<String> = (0..RACING_ATTACKS).map(|i| format!("m{i}")).collect();
    let (repointed, attacks) = tokio::join!(
        act(
            &state,
            "owner",
            "Tallies('t1')/Temper.Stale.Repoint",
            json!({ "project_id": "p2" }),
        ),
        join_all(attempts.iter().map(|last| {
            act(
                &state,
                "mallory",
                "Tallies('t1')/Temper.Stale.Bump",
                json!({ "last": last }),
            )
        })),
    );

    let mut trace = vec![format!("repoint: {}", repointed.0)];
    trace.extend(
        attempts
            .iter()
            .zip(&attacks)
            .map(|(last, (status, _))| format!("{last}: {status}")),
    );
    if repointed.0 != StatusCode::OK {
        return Err(format!(
            "the owner's repoint did not land: {} {}; {}",
            repointed.0,
            repointed.1,
            trace.join(", ")
        ));
    }
    if let Some((last, (status, body))) = attempts
        .iter()
        .zip(&attacks)
        .find(|(_, (status, _))| *status != StatusCode::OK && *status != StatusCode::FORBIDDEN)
    {
        return Err(format!(
            "Mallory's {last} was answered {status} {body}, neither landed nor refused by \
             the policy; {}",
            trace.join(", ")
        ));
    }

    let record = tally(&state, "t1").await;
    let actions: Vec<&str> = record.events.iter().map(|e| e.action.as_str()).collect();
    let Some(repoint_at) = actions.iter().position(|a| *a == "Repoint") else {
        return Err(format!("no Repoint in the record's moves {actions:?}"));
    };
    if actions[repoint_at..].contains(&"Bump") {
        return Err(format!(
            "Mallory's move landed after the tally's project was Closed: moves {actions:?}; {}",
            trace.join(", ")
        ));
    }
    let landed = attacks.iter().filter(|(s, _)| *s == StatusCode::OK).count();
    let bumps = actions.iter().filter(|a| **a == "Bump").count();
    if landed != bumps {
        return Err(format!(
            "{landed} bumps answered OK but {bumps} landed: {actions:?}"
        ));
    }
    Ok(())
}

/// A person settles the tally while runs acting for them bump it.
async fn person_and_runs_race(seed: u64) -> Result<(), String> {
    let _sim = install_deterministic_context(seed);
    let state = server(seed, "person-and-runs");
    create(&state, "Projects", json!({ "Id": "p1" })).await?;
    create(&state, "Tallies", json!({ "Id": "t1", "project_id": "p1" })).await?;

    let _faults = install_buggify_context(seed);
    let runs: Vec<String> = (0..RACING_ATTACKS).map(|i| format!("run{i}")).collect();
    let (settled, bumps) = tokio::join!(
        act(
            &state,
            "owner",
            "Tallies('t1')/Temper.Stale.Settle",
            json!({})
        ),
        join_all(runs.iter().map(|id| {
            act_as_run(
                &state,
                id,
                "Tallies('t1')/Temper.Stale.Bump",
                json!({ "last": id }),
            )
        })),
    );
    if settled.0 != StatusCode::OK {
        return Err(format!(
            "the person's settle did not land: {} {}",
            settled.0, settled.1
        ));
    }
    for (id, (status, body)) in runs.iter().zip(&bumps) {
        if *status != StatusCode::OK && !refused_by_guard(*status, body) {
            return Err(format!(
                "{id}'s bump was answered {status} {body}, neither landed nor refused by its guard"
            ));
        }
    }
    let record = tally(&state, "t1").await;
    let actions: Vec<&str> = record.events.iter().map(|e| e.action.as_str()).collect();
    let Some(settled_at) = actions.iter().position(|a| *a == "Settle") else {
        return Err(format!("no Settle in the record's moves {actions:?}"));
    };
    if actions[settled_at..].contains(&"Bump") {
        return Err(format!("a bump landed after the settle: {actions:?}"));
    }
    let landed = bumps.iter().filter(|(s, _)| *s == StatusCode::OK).count();
    let count = record.counters.get("count").copied().unwrap_or_default();
    if count != landed {
        return Err(format!(
            "{landed} bumps answered OK but the count is {count}"
        ));
    }
    Ok(())
}

/// N field updates at once on one record: every one lands.
async fn field_updates_race(seed: u64) -> Result<(), String> {
    let _sim = install_deterministic_context(seed);
    let state = server(seed, "patch");
    create(&state, "Projects", json!({ "Id": "p1" })).await?;
    create(&state, "Tallies", json!({ "Id": "t1", "project_id": "p1" })).await?;

    let _faults = install_buggify_context(seed);
    let who: Vec<String> = (0..RACING_MOVES).map(|i| format!("c{i}")).collect();
    let answers = join_all(who.iter().map(|id| {
        let request = Request::patch("/tdata/Tallies('t1')")
            .header("Content-Type", "application/json")
            .body(Body::from(json!({ "last": id }).to_string()))
            .unwrap();
        send(&state, id, request)
    }))
    .await;

    let refused: Vec<String> = who
        .iter()
        .zip(&answers)
        .filter(|(_, (status, _))| !status.is_success())
        .map(|(id, (status, body))| format!("{id}: {status} {body}"))
        .collect();
    if !refused.is_empty() {
        return Err(format!(
            "{} of {RACING_MOVES} allowed field updates did not land: {}",
            refused.len(),
            refused.join("; ")
        ));
    }
    let record = tally(&state, "t1").await;
    let last = record
        .fields
        .get("last")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !who.iter().any(|id| id == last) {
        return Err(format!(
            "after the updates `last` is {last:?}, none of theirs"
        ));
    }
    Ok(())
}

/// The owner deletes a record while others move it: the delete lands.
async fn delete_races_moves(seed: u64) -> Result<(), String> {
    let _sim = install_deterministic_context(seed);
    let state = server(seed, "delete");
    create(&state, "Projects", json!({ "Id": "p1" })).await?;
    create(&state, "Tallies", json!({ "Id": "t1", "project_id": "p1" })).await?;

    let _faults = install_buggify_context(seed);
    let who: Vec<String> = (0..RACING_ATTACKS).map(|i| format!("c{i}")).collect();
    let delete = Request::delete("/tdata/Tallies('t1')")
        .body(Body::empty())
        .unwrap();
    let (deleted, _moves) = tokio::join!(
        send(&state, "owner", delete),
        join_all(who.iter().map(|id| {
            act(
                &state,
                id,
                "Tallies('t1')/Temper.Stale.Bump",
                json!({ "last": id }),
            )
        })),
    );
    if !deleted.0.is_success() {
        return Err(format!(
            "the owner's delete did not land: {} {}",
            deleted.0, deleted.1
        ));
    }
    Ok(())
}

/// Two deletes race moves on one record: each is answered as done (2xx) or
/// not found (404), never as decided on a changed record, and the record is
/// deleted once.
async fn deletes_race(seed: u64) -> Result<(), String> {
    let _sim = install_deterministic_context(seed);
    let (state, journal) = server_with_journal(seed, "deletes");
    create(&state, "Projects", json!({ "Id": "p1" })).await?;
    create(&state, "Tallies", json!({ "Id": "t1", "project_id": "p1" })).await?;

    let _faults = install_buggify_context(seed);
    let who: Vec<String> = (0..RACING_ATTACKS).map(|i| format!("c{i}")).collect();
    let delete = || {
        Request::delete("/tdata/Tallies('t1')")
            .body(Body::empty())
            .unwrap()
    };
    let (first, second, _moves) = tokio::join!(
        send(&state, "owner", delete()),
        send(&state, "owner", delete()),
        join_all(who.iter().map(|id| {
            act(
                &state,
                id,
                "Tallies('t1')/Temper.Stale.Bump",
                json!({ "last": id }),
            )
        })),
    );
    for (status, body) in [&first, &second] {
        if !(status.is_success() || *status == StatusCode::NOT_FOUND) {
            return Err(format!("a delete was answered {status} {body}"));
        }
    }
    let tombstones = journal
        .read_events("default:Tally:t1", 0)
        .await
        .map_err(|error| format!("reading the journal: {error}"))?
        .into_iter()
        .filter(|envelope| envelope.payload["action"] == "Deleted")
        .count();
    if tombstones != 1 {
        return Err(format!("the record was deleted {tombstones} times"));
    }
    Ok(())
}

#[tokio::test]
async fn dst_stale_writes_every_allowed_move_on_a_busy_record_lands() {
    for seed in seeds() {
        if let Err(violation) = allowed_moves_race(seed).await {
            panic!("seed {seed}: {violation}");
        }
    }
}

#[tokio::test]
async fn dst_stale_writes_a_person_and_their_runs_moving_one_record_are_never_answered_stale() {
    for seed in seeds() {
        if let Err(violation) = person_and_runs_race(seed).await {
            panic!("seed {seed}: {violation}");
        }
    }
}

#[tokio::test]
async fn dst_stale_writes_a_move_never_lands_on_a_state_its_policy_refuses() {
    for seed in seeds() {
        if let Err(violation) = linked_record_attack(seed).await {
            panic!("seed {seed}: {violation}");
        }
    }
}

#[tokio::test]
async fn dst_stale_writes_every_allowed_field_update_on_a_busy_record_lands() {
    for seed in seeds() {
        if let Err(violation) = field_updates_race(seed).await {
            panic!("seed {seed}: {violation}");
        }
    }
}

#[tokio::test]
async fn dst_stale_writes_a_delete_lands_while_the_record_is_being_moved() {
    for seed in seeds() {
        if let Err(violation) = delete_races_moves(seed).await {
            panic!("seed {seed}: {violation}");
        }
    }
}

#[tokio::test]
async fn dst_stale_writes_racing_deletes_delete_once() {
    for seed in seeds() {
        if let Err(violation) = deletes_race(seed).await {
            panic!("seed {seed}: {violation}");
        }
    }
}
