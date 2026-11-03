//! The simulation clock of the state-timer simulations: what a server
//! process's paused clock reads, carried across restarts.

use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use temper_runtime::scheduler::SimClock;

/// Simulation time: the paused clock of the running process, carried across
/// restarts with the time the server was down.
pub(crate) struct ProcessClock {
    epoch: DateTime<Utc>,
    base: Mutex<(Option<tokio::time::Instant>, Duration)>,
}

impl ProcessClock {
    /// A clock at the simulation epoch, before any process ran.
    pub(crate) fn new() -> Self {
        Self {
            epoch: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            base: Mutex::new((None, Duration::ZERO)),
        }
    }

    /// Start counting from this process's clock. Call inside its runtime.
    pub(crate) fn begin(&self) {
        self.base.lock().unwrap().0 = Some(tokio::time::Instant::now());
    }

    /// Stop counting, and add `downtime` before the next process. Call inside
    /// the ending process's runtime.
    pub(crate) fn end(&self, downtime: Duration) {
        let mut base = self.base.lock().unwrap();
        let ran = base
            .0
            .take()
            .map(|origin| origin.elapsed())
            .unwrap_or_default();
        base.1 += ran + downtime;
    }
}

impl SimClock for ProcessClock {
    fn now(&self) -> DateTime<Utc> {
        let base = self.base.lock().unwrap();
        let ran = base.0.map(|origin| origin.elapsed()).unwrap_or_default();
        self.epoch + chrono::Duration::from_std(base.1 + ran).unwrap()
    }

    fn tick(&self) -> u64 {
        0
    }
}
