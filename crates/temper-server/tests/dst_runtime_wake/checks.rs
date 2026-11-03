//! The wake simulation's invariants, read from the journal.

use chrono::{DateTime, Utc};
use temper_runtime::persistence::EventStore;
use temper_store_sim::SimEventStore;

use super::{LATE, PULSE, TENANT, TICK, TIMEOUT};

/// The invariant a slept-through or late timer breaks.
pub(crate) const ON_TIME: &str =
    "every timer fires at most 10 s after it is due, and none is slept through";

/// How a record of the workload started.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Start {
    /// `Fire` is due then.
    Plan { due: DateTime<Utc> },
    /// `Expire` is due an hour after it.
    Wait,
    /// `Tick` is due two hours after it, and two hours after each `Tick`.
    Activate,
    /// `Pulse` is due a minute after it, and a minute after each `Pulse`.
    Hum,
}

/// One record of the workload.
pub(crate) struct Rec {
    pub(crate) id: String,
    pub(crate) start: Start,
}

type Broken = (&'static str, String);

/// The record's events, in order: which action, and when it committed.
async fn journal_of(events: &SimEventStore, id: &str) -> Vec<(String, DateTime<Utc>)> {
    events
        .read_events(&format!("{TENANT}:Job:{id}"), 0)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|event| (event.event_type, event.metadata.timestamp))
        .collect()
}

fn committed(log: &[(String, DateTime<Utc>)], action: &str) -> Vec<DateTime<Utc>> {
    log.iter()
        .filter(|(event, _)| event == action)
        .map(|(_, at)| *at)
        .collect()
}

/// A one-shot timer due `due` fired once, on time, or is not due yet.
fn once(
    id: &str,
    fired: &[DateTime<Utc>],
    due: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<(), Broken> {
    if fired.len() > 1 {
        return Err((
            "nothing fires twice",
            format!("{id} due {due} fired at {fired:?}"),
        ));
    }
    match fired.first() {
        Some(at) if *at < due => Err(("nothing fires early", format!("{id} due {due} fired {at}"))),
        Some(at) if (*at - due).num_seconds() > LATE => {
            Err((ON_TIME, format!("{id} due {due} fired {at}")))
        }
        None if (end - due).num_seconds() > LATE => {
            Err((ON_TIME, format!("{id} due {due} never fired by {end}")))
        }
        _ => Ok(()),
    }
}

/// A chain started at `from` fired every `period` after its last fire, on
/// time, and once each.
fn chain(
    id: &str,
    from: DateTime<Utc>,
    fired: &[DateTime<Utc>],
    period: i64,
    end: DateTime<Utc>,
) -> Result<(), Broken> {
    let mut last = from;
    for at in fired {
        let due = last + chrono::Duration::seconds(period);
        if *at < due {
            return Err((
                "nothing fires twice",
                format!("{id} due {due} fired {at}, after {last}"),
            ));
        }
        if (*at - due).num_seconds() > LATE {
            return Err((ON_TIME, format!("{id} due {due} fired {at}")));
        }
        last = *at;
    }
    let due = last + chrono::Duration::seconds(period);
    if (end - due).num_seconds() > LATE {
        return Err((ON_TIME, format!("{id} due {due} never fired by {end}")));
    }
    Ok(())
}

/// Every timer of the workload fired once, on time, by `end`.
pub(crate) async fn journal(
    events: &SimEventStore,
    recs: &[Rec],
    end: DateTime<Utc>,
) -> Result<(), Broken> {
    for rec in recs {
        let log = journal_of(events, &rec.id).await;
        let started = |action: &str| committed(&log, action).first().copied().unwrap();
        match rec.start {
            Start::Plan { due } => once(&rec.id, &committed(&log, "Fire"), due, end)?,
            Start::Wait => {
                let due = started("Wait") + chrono::Duration::seconds(TIMEOUT);
                once(&rec.id, &committed(&log, "Expire"), due, end)?;
            }
            Start::Activate => chain(
                &rec.id,
                started("Activate"),
                &committed(&log, "Tick"),
                TICK,
                end,
            )?,
            Start::Hum => chain(
                &rec.id,
                started("Hum"),
                &committed(&log, "Pulse"),
                PULSE,
                end,
            )?,
        }
    }
    Ok(())
}
