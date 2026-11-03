//! Timers in a process that can be suspended: they wait by the wall clock,
//! and the process reports the earliest due time it could sleep through.
//!
//! A sandbox that holds this process may be suspended while nothing calls it,
//! and the process's monotonic clock, which tokio's sleeps run on, stands
//! still meanwhile. So every timer waits until its wall-clock due time, and a
//! check once a second ([`DueTimes::start`]) sees a resume and makes every
//! waiting timer compute its wait again: one that fell due while the process
//! slept fires within about a second of the resume.
//!
//! The process also tells the control plane when it next has work it could
//! sleep through: the earliest due time of its timers (state timers and
//! scheduled actions) at or after the time it is surely awake until, which is
//! [`ServerState::set_wake_awake`] after its newest inbound request. It sends
//! a [`WakeReport`] to its [`WakeSink`] when that time comes earlier than what
//! the control plane holds, or when the time the control plane holds has
//! passed and there is a next one. At that time the control plane sends the
//! process any request (`GET /healthz` will do), which resumes the sandbox.

mod due;
mod http;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use chrono::{DateTime, Utc};
use serde::Serialize;

pub(crate) use due::{Armed, DueTimes, TimerRef};
pub use http::HttpWakeSink;

use crate::state::ServerState;

/// What the process tells the control plane: when it next has work. The
/// control plane keeps the newest by `(reported_at, seq)`; `seq` counts this
/// process's reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WakeReport {
    /// The earliest due time the process could sleep through; None when
    /// nothing waits that long.
    pub next_due_at: Option<DateTime<Utc>>,
    /// The process's clock when it made the report.
    pub reported_at: DateTime<Utc>,
    pub seq: u64,
}

/// Where reports go: the control plane, or a simulation's record of them.
#[async_trait::async_trait]
pub trait WakeSink: Send + Sync {
    /// Deliver one report, with the W3C `traceparent` of the move that
    /// changed the earliest due time.
    async fn send(&self, report: &WakeReport, traceparent: Option<&str>) -> Result<(), SendError>;
}

/// Why a report was not delivered, which decides whether it is sent again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendError {
    /// It did not get through: the network, a timeout, or a control plane
    /// that failed or asked for a later try. Sent again, waiting longer each
    /// time.
    Undelivered(String),
    /// The control plane refused it on purpose, with this status: the
    /// process's pass is bad or revoked, as when the app was deleted or its
    /// pass replaced. Not sent again, since a refused pass is refused on every
    /// retry; an earlier due time, or this one passing, sends a new report, so
    /// a pass that comes back works again.
    Refused(u16),
}

impl ServerState {
    /// Send reports to `sink` from now on. Call before
    /// [`Self::start_runtime`], which reports once the boot's timers are armed.
    pub fn set_wake_sink(&self, sink: std::sync::Arc<dyn WakeSink>) {
        self.due_times.set_sink(sink);
    }

    /// The sandbox stays awake for `awake` after each inbound request, so a
    /// timer due before then fires on its own and is not reported. Unset, the
    /// process may be suspended at any moment and reports its earliest due
    /// time from now. Set it below the time the sandbox really stays awake,
    /// or a timer due in between is slept through.
    pub fn set_wake_awake(&self, awake: std::time::Duration) {
        self.due_times.set_awake(awake);
    }

    /// An inbound request came: the sandbox stays awake from now.
    pub fn note_inbound(&self) {
        self.due_times.note_inbound();
    }
}

/// Note every request that reaches the kernel's routes as inbound, whatever it
/// asks and however it is answered: any request keeps the sandbox awake.
pub(crate) async fn note_inbound(
    State(state): State<ServerState>,
    request: Request,
    next: Next,
) -> Response {
    state.note_inbound();
    next.run(request).await
}
