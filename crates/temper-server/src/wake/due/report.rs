//! Reporting: the due time the process could sleep through, sent to the
//! control plane when it is earlier than what the control plane holds, or
//! once what it holds has passed.

use std::sync::Arc;
use std::time::Duration;

use temper_runtime::scheduler::sim_now;

use super::DueTimes;
use crate::wake::{SendError, WakeReport};

/// The wait before an undelivered report is sent again; it doubles with each
/// try, up to [`LAST_RETRY_WAIT`].
const FIRST_RETRY_WAIT: Duration = Duration::from_secs(1);

/// The longest wait between tries: a control plane that is down for long is
/// tried about once an hour, not hammered.
const LAST_RETRY_WAIT: Duration = Duration::from_secs(3600);

impl DueTimes {
    /// Send the due time the process could sleep through to the sink when
    /// the control plane was told nothing yet, or when there is one and the
    /// control plane holds no later wake before it: the last report named
    /// none, named a later time, or named a time that has passed (the
    /// control plane woke the process then, or is about to, and holds nothing
    /// after it).
    ///
    /// A later time is not sent: the control plane wakes the process at the
    /// time it holds, early for nothing, and that time passing sends the
    /// fresh one. Nor is none, once the time last sent has passed: the
    /// control plane holds nothing after it either way. And while the control
    /// plane's wake at a passed time keeps the process awake, a due time
    /// before the end of that wake's window is not sent. So a timer that
    /// keeps re-arming costs one report and one wake per window the sandbox
    /// stays awake, not one per fire.
    ///
    /// A report that is not delivered is sent again, waiting longer after
    /// each failure, or at once after a resume, until it is delivered or a
    /// newer report replaces it: without it the control plane would not wake
    /// the process for that due time. A report the control plane refuses on
    /// purpose ([`SendError::Refused`]) counts as told and is not sent again:
    /// its pass would be refused on every retry. The report carries the
    /// current span's trace: the move that changed the due time.
    pub(crate) fn report(self: &Arc<Self>) {
        let Some(sink) = self
            .sink
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        else {
            return;
        };
        let report = {
            let mut inner = self.lock();
            let now = sim_now();
            // The control plane's wake at a time that has passed keeps the
            // process awake while nothing is sent. A report that is sent
            // replaces that time and may reach the control plane before the
            // wake goes out, so it names what the process could sleep through
            // without that wake.
            let woken = inner.passed_wake(now);
            let soonest = inner.reportable(inner.horizon(now, woken), now);
            let send = match inner.reported.sent {
                None => true,
                Some(sent) => {
                    soonest.is_some_and(|at| sent.is_none_or(|sent| sent <= now || at < sent))
                }
            };
            if !send {
                return;
            }
            let earliest = match woken {
                Some(_) => inner.reportable(inner.horizon(now, None), now),
                None => soonest,
            };
            inner.reported.seq += 1;
            let seq = inner.reported.seq;
            inner.reported.sent = Some(earliest);
            inner.reported.in_flight = Some(seq);
            WakeReport {
                next_due_at: earliest,
                reported_at: now,
                seq,
            }
        };
        let traceparent = crate::trace_context::traceparent(&tracing::Span::current());
        let due = self.clone();
        let send = async move {
            let mut wait = FIRST_RETRY_WAIT;
            loop {
                // A report replaced while it waited to be tried again is not
                // sent: what replaced it is newer.
                if due.lock().reported.in_flight != Some(report.seq) {
                    return;
                }
                let result = sink.send(&report, traceparent.as_deref()).await;
                let error = {
                    let mut inner = due.lock();
                    if inner.reported.in_flight != Some(report.seq) {
                        // A newer report replaced this one while it was sent.
                        return;
                    }
                    inner.reported.in_flight = None;
                    match result {
                        Ok(()) => {
                            inner.reported.taken = report.next_due_at;
                            return;
                        }
                        Err(SendError::Refused(status)) => {
                            drop(inner);
                            tracing::warn!(
                                seq = report.seq,
                                status,
                                "wake report refused by the control plane; not sent again"
                            );
                            return;
                        }
                        Err(SendError::Undelivered(error)) => {
                            inner.reported.in_flight = Some(report.seq);
                            error
                        }
                    }
                };
                tracing::warn!(seq = report.seq, %error, retry_in_ms = wait.as_millis() as u64,
                    "wake report not delivered; retrying");
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {} // determinism-ok: retry backoff
                    _ = due.resumed.notified() => {}
                }
                wait = (wait * 2).min(LAST_RETRY_WAIT);
            }
        };
        tokio::spawn(send); // determinism-ok: a report's delivery runs beside the commit
    }
}
