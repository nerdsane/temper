//! Cooperative fault points on the append path.
//!
//! An append passes three points: its transaction began, its rows are
//! written, and its commit returned. A test sets a [`CommitProbe`] on the
//! store to pause one append at one point, so it can make a timeout fire or a
//! caller hang up there. Outside tests the points cost nothing.

use std::time::Duration;

use super::TursoEventStore;

/// Where an append is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CommitPoint {
    /// The write transaction began: the write lock is held.
    Began,
    /// Every row is written; nothing is committed yet.
    Wrote,
    /// The commit returned.
    Committed,
}

impl CommitPoint {
    /// Every point, in the order an append passes them.
    #[cfg(test)]
    pub(crate) const ALL: [Self; 3] = [Self::Began, Self::Wrote, Self::Committed];
}

/// Pause the first append that reaches `at` for `pause`, or every one.
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct CommitProbe {
    at: CommitPoint,
    pause: Duration,
    /// Pause only the first append to get there.
    once: bool,
    /// How long one append attempt may take before it is retried.
    attempt_timeout: Duration,
    fired: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(test)]
impl CommitProbe {
    pub(crate) fn new(at: CommitPoint, pause: Duration, attempt_timeout: Duration) -> Self {
        Self {
            at,
            pause,
            once: true,
            attempt_timeout,
            fired: Default::default(),
        }
    }

    /// Pause every append that reaches `at`, as a slow drive would.
    pub(crate) fn every(at: CommitPoint, pause: Duration, attempt_timeout: Duration) -> Self {
        Self {
            once: false,
            ..Self::new(at, pause, attempt_timeout)
        }
    }

    /// Whether an append has been paused yet.
    pub(crate) fn fired(&self) -> bool {
        self.fired.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
impl std::fmt::Debug for CommitProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommitProbe")
            .field("at", &self.at)
            .field("pause", &self.pause)
            .finish()
    }
}

impl TursoEventStore {
    /// Pass `point`: a test's probe may pause the append here.
    pub(super) async fn commit_point(&self, point: CommitPoint) {
        #[cfg(test)]
        if let Some(probe) = &self.commit_probe
            && point == probe.at
            && (!probe.fired.swap(true, std::sync::atomic::Ordering::SeqCst) || !probe.once)
        {
            tokio::time::sleep(probe.pause).await;
        }
        let _ = point;
    }

    /// How long one append attempt may take before it is retried.
    pub(super) fn append_attempt_timeout(&self) -> Duration {
        #[cfg(test)]
        if let Some(probe) = &self.commit_probe {
            return probe.attempt_timeout;
        }
        super::append_config::append_attempt_timeout()
    }

    /// Pause appends as `probe` says.
    #[cfg(test)]
    pub(crate) fn with_commit_probe(mut self, probe: CommitProbe) -> Self {
        self.commit_probe = Some(probe);
        self
    }
}
