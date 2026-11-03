//! The invariants, checked from the journal and the timer rows alone.

use temper_runtime::persistence::EventStore;
use temper_server::ServerState;
use temper_server::storage::StateTimerStore;

use super::logged::Withheld;
use super::server::World;
use super::{Record, TENANT, Timer, fail};

/// Check every record against the invariants, from its journal and rows
/// alone: a point read could rehydrate an actor and arm its timer again.
pub(super) async fn check(
    seed: u64,
    trace: &[String],
    world: &World,
    state: &ServerState,
    records: &[Record],
    withheld: &Withheld,
    when: &str,
) -> Result<(), String> {
    let scheduled = world.events.list_scheduled_actions(TENANT).await.unwrap();
    let armed = state.armed_scheduled_actions();
    for record in records {
        let (_, fired, waits_in, moves_to) = record.timer.moves();
        let entity_type = record.timer.entity_type();
        let journal = world
            .events
            .read_events(&format!("{TENANT}:{entity_type}:{}", record.id), 0)
            .await
            .map_err(|error| fail(seed, trace, "journals are readable", error.to_string()))?;
        let moves = journal
            .iter()
            .filter(|event| event.event_type == fired)
            .count();
        let place = journal
            .last()
            .and_then(|event| event.payload.get("to_status"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        // The shared rows read a `Job`'s directly; any other type's through
        // the store.
        let row = match entity_type {
            "Job" => world.rows.row(TENANT, &record.id),
            _ => world
                .rows
                .state_timer_row(TENANT, entity_type, &record.id)
                .await
                .map_err(|error| fail(seed, trace, "timer rows are readable", error.to_string()))?,
        };
        let scheduled_row = scheduled.iter().any(|row| row.entity_id == record.id);
        let live = armed.keys().any(|fire| fire.entity_id == record.id);
        let logged = withheld.of(&record.id);
        let detail = format!(
            "{when}: {} {:?} {:?}: {fired} made {moves} times, in {place}, state timer row {:?}, \
             scheduled row {scheduled_row}, live scheduled timer {live}, withheld as {logged:?}",
            record.id,
            record.setter,
            record.timer,
            row.as_ref().map(|row| (&row.state, &row.armed_by)),
        );
        if !record.setter.loses_standing() {
            if moves != 1 || place != moves_to {
                return Err(fail(
                    seed,
                    trace,
                    "a timer whose setter has standing, or that acts for no one, makes its move once",
                    detail,
                ));
            }
            continue;
        }
        if moves != 0 || place != waits_in {
            return Err(fail(
                seed,
                trace,
                "a timer whose setter has no standing never makes its move",
                detail,
            ));
        }
        match record.timer {
            Timer::State | Timer::Created => {
                let as_setter = row.as_ref().is_some_and(|row| {
                    row.state == waits_in
                        && row
                            .armed_by
                            .contains(&format!("\"{}\"", record.setter.principal(&record.user)))
                        && row.armed_by.contains(&format!("\"{}\"", record.user))
                });
                if !as_setter {
                    return Err(fail(
                        seed,
                        trace,
                        "a withheld state timer keeps its row, armed as its setter",
                        detail,
                    ));
                }
            }
            Timer::Scheduled => {
                if scheduled_row || live {
                    return Err(fail(
                        seed,
                        trace,
                        "a withheld scheduled action leaves no row and no live timer",
                        detail,
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Check that each leaver's withheld fire was logged, naming its setter.
pub(super) fn check_logged(
    seed: u64,
    trace: &[String],
    records: &[Record],
    withheld: &Withheld,
) -> Result<(), String> {
    for record in records {
        let logged = withheld.of(&record.id);
        let principal = record.setter.principal(&record.user);
        let named = !logged.is_empty() && logged.iter().all(|setter| *setter == principal);
        if record.setter.loses_standing() != named {
            return Err(fail(
                seed,
                trace,
                "only a fire withheld for its setter's standing is logged, naming the setter",
                format!("{} {:?}: logged {logged:?}", record.id, record.setter),
            ));
        }
    }
    Ok(())
}
