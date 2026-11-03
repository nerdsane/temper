//! When each live timer of this process is due, and the waits that run by the
//! wall clock.
//!
//! Every state timer and scheduled action registers here when its task starts
//! waiting ([`DueTimes::arm`]), moves its due time when a fire that did not
//! commit waits to try again ([`DueTimes::retry_in`]), and leaves when its
//! task ends ([`DueTimes::end`]). A timer waits until its wall-clock due time
//! ([`Armed::wait_until`]): for the time left, on the process's monotonic
//! clock, computed again after a resume.
//!
//! A watch ([`DueTimes::start`]) compares, every [`WATCH_EVERY`], how far the
//! wall clock and the monotonic clock moved. When the wall clock ran more than
//! [`RESUME_GAP`] ahead, the process was suspended: every waiting timer
//! computes its wait again, and one that fell due meanwhile fires at once. It
//! does not matter what resumed the sandbox. The watch also runs the report
//! rule, so a due time the control plane holds that passes with nothing else
//! happening in the process still brings the next report.
//!
//! The process reports only the due times it could sleep through. Its sandbox
//! stays awake for a while after each inbound request ([`DueTimes::note_inbound`]),
//! and a timer due before then fires while it is awake, so the report names
//! the earliest due time at or after that horizon.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use chrono::{DateTime, Utc};
use temper_runtime::scheduler::sim_now;
use tokio::sync::Notify;

use super::WakeSink;
use crate::request_context::ScheduledFire;

/// How often the watch checks for a resume and runs the report rule. A timer
/// that fell due while the process was suspended fires within this of the
/// resume.
const WATCH_EVERY: Duration = Duration::from_secs(1);

/// How far the wall clock may run ahead of the process's monotonic clock in
/// one check before the process counts itself resumed from a suspension. It
/// sits above scheduler jitter in one check, which moves both clocks alike, so
/// a process that kept running never re-times its timers.
const RESUME_GAP: Duration = Duration::from_secs(1);

/// One live timer: a record's state timer (a record has at most one), or one
/// scheduled-action row.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum TimerRef {
    State {
        tenant: String,
        entity_type: String,
        entity_id: String,
    },
    Scheduled(ScheduledFire),
}

impl TimerRef {
    /// The least key, which sorts before every real one: a state timer with
    /// empty names, as the derived order puts `State` first.
    fn least() -> Self {
        Self::State {
            tenant: String::new(),
            entity_type: String::new(),
            entity_id: String::new(),
        }
    }
}

/// A timer's registration, held by its task.
pub(crate) struct Armed {
    key: TimerRef,
    id: u64,
    resumed: Arc<Notify>,
}

struct Entry {
    id: u64,
    at: DateTime<Utc>,
}

/// What the control plane was told, and what is on its way.
#[derive(Default)]
struct Reported {
    seq: u64,
    /// The due time of the newest report sent, delivered, on its way or
    /// refused: what the control plane holds as far as the process knows.
    /// None before the first report.
    sent: Option<Option<DateTime<Utc>>>,
    /// The seq of the newest report not yet delivered, sent again until it is.
    in_flight: Option<u64>,
    /// The due time of the newest report the control plane took: it wakes
    /// the process then.
    taken: Option<DateTime<Utc>>,
}

#[derive(Default)]
struct Inner {
    timers: BTreeMap<TimerRef, Entry>,
    by_due: BTreeSet<(DateTime<Utc>, TimerRef)>,
    next_id: u64,
    reported: Reported,
    /// How long the sandbox stays awake after an inbound request; None: it
    /// may be suspended at any moment.
    awake: Option<chrono::Duration>,
    /// When the newest inbound request came.
    last_inbound: Option<DateTime<Utc>>,
}

impl Inner {
    /// The due time the control plane took, once it has passed, while the
    /// control plane still wakes the process for it: nothing newer was sent
    /// since, which would replace it before that wake went out. That wake is
    /// an inbound request, whether it has come yet or is on its way, and even
    /// when the process does not see it, as with `GET /healthz`.
    fn passed_wake(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let taken = self.reported.taken.filter(|at| *at <= now)?;
        (self.reported.in_flight.is_none() && self.reported.sent == Some(Some(taken)))
            .then_some(taken)
    }

    /// Until when the process is surely awake at `now`, from the newest
    /// inbound request it saw, or `woken` when that is newer: a timer due
    /// before then fires on its own. Now, unless the sandbox is known to stay
    /// awake longer.
    fn horizon(&self, now: DateTime<Utc>, woken: Option<DateTime<Utc>>) -> DateTime<Utc> {
        match (self.awake, self.last_inbound.max(woken)) {
            (Some(awake), Some(last)) => last
                .checked_add_signed(awake)
                .unwrap_or(DateTime::<Utc>::MAX_UTC)
                .max(now),
            _ => now,
        }
    }

    /// The earliest due time at or after `horizon` and after `now`: the one
    /// the process could sleep through. A timer due by now is firing while
    /// the process runs, and one due past the end of the calendar never falls
    /// due, so neither is reported.
    fn reportable(&self, horizon: DateTime<Utc>, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.by_due
            .range((horizon, TimerRef::least())..)
            .map(|(at, _)| *at)
            .find(|at| *at > now)
            .filter(|at| *at < DateTime::<Utc>::MAX_UTC)
    }

    fn remove(&mut self, key: &TimerRef) -> Option<Entry> {
        let entry = self.timers.remove(key)?;
        self.by_due.remove(&(entry.at, key.clone()));
        Some(entry)
    }
}

/// The due time of every live timer in this process.
#[derive(Default)]
pub(crate) struct DueTimes {
    inner: Mutex<Inner>,
    sink: Mutex<Option<Arc<dyn WakeSink>>>,
    /// Tells every waiting timer, and a report waiting to be tried again, that
    /// the process resumed from a suspension.
    resumed: Arc<Notify>,
}

impl DueTimes {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Where reports go. Without a sink, nothing is reported.
    pub(crate) fn set_sink(&self, sink: Arc<dyn WakeSink>) {
        *self
            .sink
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(sink);
    }

    /// The sandbox stays awake for `awake` after each inbound request.
    pub(crate) fn set_awake(&self, awake: Duration) {
        self.lock().awake = chrono::Duration::from_std(awake).ok();
    }

    /// The runtime started: report, and watch for resumes from now on. The
    /// watch holds the index weakly, so it ends with the server.
    pub(crate) fn start(self: &Arc<Self>) {
        self.report();
        // determinism-ok: the watch runs on the runtime's clock, as timers do;
        // under simulation, on the single-threaded test runtime.
        tokio::spawn(watch(Arc::downgrade(self)));
    }

    /// An inbound request came, which keeps the sandbox awake.
    pub(crate) fn note_inbound(self: &Arc<Self>) {
        let now = sim_now();
        {
            let mut inner = self.lock();
            inner.last_inbound = inner.last_inbound.max(Some(now));
        }
        self.report();
    }

    /// Register `key`'s timer, due `at`, replacing the one it had: a newer
    /// arm of the same record's state timer supersedes the older one.
    pub(crate) fn arm(self: &Arc<Self>, key: TimerRef, at: DateTime<Utc>) -> Armed {
        let id = {
            let mut inner = self.lock();
            inner.next_id += 1;
            let id = inner.next_id;
            inner.remove(&key);
            inner.by_due.insert((at, key.clone()));
            inner.timers.insert(key.clone(), Entry { id, at });
            id
        };
        self.report();
        Armed {
            key,
            id,
            resumed: self.resumed.clone(),
        }
    }

    /// `armed`'s timer did not commit and tries again after `wait`: when that
    /// is by the wall clock, which is when it is due now. A wait past the end
    /// of the calendar never falls due.
    pub(crate) fn retry_in(self: &Arc<Self>, armed: &Armed, wait: Duration) -> DateTime<Utc> {
        let at = chrono::Duration::from_std(wait)
            .ok()
            .and_then(|wait| sim_now().checked_add_signed(wait))
            .unwrap_or(DateTime::<Utc>::MAX_UTC);
        {
            let mut inner = self.lock();
            if inner
                .timers
                .get(&armed.key)
                .is_none_or(|entry| entry.id != armed.id)
            {
                return at;
            }
            inner.remove(&armed.key);
            inner.by_due.insert((at, armed.key.clone()));
            inner
                .timers
                .insert(armed.key.clone(), Entry { id: armed.id, at });
        }
        self.report();
        at
    }

    /// `armed`'s task ended. A newer arm of the same timer keeps its entry.
    pub(crate) fn end(self: &Arc<Self>, armed: &Armed) {
        {
            let mut inner = self.lock();
            if inner
                .timers
                .get(&armed.key)
                .is_none_or(|entry| entry.id != armed.id)
            {
                return;
            }
            inner.remove(&armed.key);
        }
        self.report();
    }
}

impl Armed {
    /// Wait until the wall clock reaches `at`: for the time left now, on the
    /// process's monotonic clock. That clock stands still while the sandbox
    /// is suspended, so the time left is computed again after a resume, and a
    /// timer that fell due meanwhile returns then.
    pub(crate) async fn wait_until(&self, at: DateTime<Utc>) {
        loop {
            // Listen before reading the clock, so a resume noticed in between
            // is not missed.
            let resumed = self.resumed.notified();
            tokio::pin!(resumed);
            resumed.as_mut().enable();
            let Ok(left) = (at - sim_now()).to_std() else {
                return;
            };
            if left.is_zero() {
                return;
            }
            tokio::select! {
                _ = tokio::time::sleep(left) => return, // determinism-ok: timer delay
                _ = resumed => {}
            }
        }
    }
}

/// Every [`WATCH_EVERY`] while the index lives: tell waiting timers when the
/// process resumed, and run the report rule.
async fn watch(due: Weak<DueTimes>) {
    let mut wall = sim_now();
    let mut clock = tokio::time::Instant::now(); // determinism-ok: the process's own clock
    loop {
        tokio::time::sleep(WATCH_EVERY).await; // determinism-ok: the process's own clock
        let Some(due) = due.upgrade() else {
            return;
        };
        let (now_wall, now_clock) = (sim_now(), tokio::time::Instant::now()); // determinism-ok: as above
        let walked = (now_wall - wall).to_std().unwrap_or_default();
        let asleep = walked.saturating_sub(now_clock.saturating_duration_since(clock));
        (wall, clock) = (now_wall, now_clock);
        if asleep > RESUME_GAP {
            tracing::info!(
                asleep_ms = asleep.as_millis() as u64,
                "process resumed from a suspension; timers wait again by the wall clock"
            );
            due.resumed.notify_waiters();
        }
        due.report();
    }
}

#[path = "due/report.rs"]
mod report;
#[cfg(test)]
#[path = "due/tests.rs"]
mod tests;
