//! A record whose message has not finished says so, and says what it waits on.
//!
//! A record handles one message at a time, so a message that never finishes
//! leaves every later ask to time out, and nothing in the log said why: on
//! 3 Nov 2026 KatagamiApp a6s03ju74p52 on the main Temper answered no ask for
//! 19 minutes, and no line named the await it was in. While a message (or the
//! record's start) runs past the dispatch layer's ask deadline, this logs
//! `handler still running` with the record, the message, the await it is in
//! and how long it has run, then again each minute until it ends, and
//! `handler finished` when it does. It is a log line from the record's own
//! task, not a span, because a span is exported only when it closes and a hung
//! await never closes one. It reports; it never ends or limits the message.

use std::future::Future;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

/// How often a message still running is reported again.
const REPORT_EVERY: Duration = Duration::from_secs(60);

/// What a record's current message is doing: every await its handler and its
/// start make, plus the work between them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum Doing {
    /// Between awaits: guards, effects, building the event.
    Deciding = 0,
    /// Reading the field blobs an action's guards compare.
    ReadingBlobs,
    /// Storing the field-overflow blobs an event refers to.
    WritingBlobs,
    /// Appending an event to the journal.
    Appending,
    /// Reading the journal to learn whether a failed append landed.
    LearningWriteOutcome,
    /// Rebuilding state from the journal: a start, or catching up after
    /// another writer moved it.
    Replaying,
    /// Reading the record's armed timers.
    ReadingTimers,
    /// Saving or queueing a snapshot.
    Snapshotting,
    /// Pausing before an append is retried.
    RetryPause,
}

impl Doing {
    const ALL: [Doing; 9] = [
        Doing::Deciding,
        Doing::ReadingBlobs,
        Doing::WritingBlobs,
        Doing::Appending,
        Doing::LearningWriteOutcome,
        Doing::Replaying,
        Doing::ReadingTimers,
        Doing::Snapshotting,
        Doing::RetryPause,
    ];

    fn name(self) -> &'static str {
        match self {
            Doing::Deciding => "deciding",
            Doing::ReadingBlobs => "reading blobs",
            Doing::WritingBlobs => "writing blobs",
            Doing::Appending => "appending to the journal",
            Doing::LearningWriteOutcome => "learning a write's outcome",
            Doing::Replaying => "replaying the journal",
            Doing::ReadingTimers => "reading timers",
            Doing::Snapshotting => "snapshotting",
            Doing::RetryPause => "pausing before a retry",
        }
    }
}

/// The record's current [`Doing`], read by the watch while the handler runs.
#[derive(Debug, Default)]
pub(super) struct Now(AtomicU8);

impl Now {
    /// Await `work` as `doing`, then go back to what was being done before.
    pub(super) async fn during<F: Future>(&self, doing: Doing, work: F) -> F::Output {
        let before = self.0.swap(doing as u8, Ordering::Relaxed);
        let _back = Back { now: self, before };
        work.await
    }

    fn get(&self) -> Doing {
        let raw = self.0.load(Ordering::Relaxed);
        Doing::ALL
            .get(usize::from(raw))
            .copied()
            .unwrap_or(Doing::Deciding)
    }
}

/// Puts the previous [`Doing`] back when an await ends, however it ends.
struct Back<'a> {
    now: &'a Now,
    before: u8,
}

impl Drop for Back<'_> {
    fn drop(&mut self) {
        self.now.0.store(self.before, Ordering::Relaxed);
    }
}

/// The record a watch reports on.
pub(super) struct Record<'a> {
    /// The tenant this record belongs to.
    pub tenant: &'a str,
    /// The record's entity type.
    pub entity_type: &'a str,
    /// The record's entity id.
    pub entity_id: &'a str,
}

/// Run `work` (one message, or the record's start) and report while it runs
/// past `ask_deadline`, the dispatcher's configured per-attempt timeout.
pub(super) async fn watched<F: Future>(
    record: Record<'_>,
    message: &str,
    ask_deadline: Duration,
    now: &Now,
    work: F,
) -> F::Output {
    let started = tokio::time::Instant::now(); // determinism-ok: production watchdog, reports only
    let mut work = std::pin::pin!(work);
    let mut next = ask_deadline;
    let mut reported = false;
    loop {
        tokio::select! {
            biased;
            out = &mut work => {
                if reported {
                    tracing::warn!(
                        tenant = %record.tenant,
                        entity_type = %record.entity_type,
                        entity_id = %record.entity_id,
                        action = message,
                        elapsed_secs = started.elapsed().as_secs(),
                        "handler finished"
                    );
                }
                return out;
            }
            () = tokio::time::sleep_until(started + next) => { // determinism-ok: production watchdog, reports only
                reported = true;
                tracing::warn!(
                    tenant = %record.tenant,
                    entity_type = %record.entity_type,
                    entity_id = %record.entity_id,
                    action = message,
                    awaiting = now.get().name(),
                    elapsed_secs = started.elapsed().as_secs(),
                    "handler still running"
                );
                next += REPORT_EVERY;
            }
        }
    }
}

#[cfg(all(test, feature = "sim"))]
#[path = "watchdog_test.rs"]
mod tests;
