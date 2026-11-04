//! How far a walk has got: every state within how many moves of a start has
//! been walked, while threads walk in no strict order.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Depths told apart. A deeper state counts as this deep, which can only make
/// [`Levels::covered`] say less than it might.
const DEPTHS: usize = 1 << 12;

/// The depth of what a state at `depth` leads to.
pub(super) fn deeper(depth: u16) -> u16 {
    (depth + 1).min(DEPTHS as u16 - 1)
}

/// Jobs left by depth, a state's depth being the moves it was found at, which
/// threads can make more than its true distance from a start.
#[derive(Debug)]
pub(in crate::composite::joint) struct Levels {
    /// Jobs not finished, by depth.
    jobs: Vec<AtomicUsize>,
    /// The deepest job made.
    deepest: AtomicUsize,
    /// No job is left nearer than this, and none can come: jobs at a depth
    /// come only from jobs one nearer.
    nearest: AtomicUsize,
    /// How much deeper than a later way to it, by the most, a state was found.
    late: AtomicUsize,
}

impl Levels {
    /// `starts` jobs wait, at depth 0.
    pub(super) fn new(starts: usize) -> Self {
        let jobs: Vec<AtomicUsize> = (0..DEPTHS).map(|_| AtomicUsize::new(0)).collect();
        jobs[0].store(starts, Ordering::Relaxed);
        Self {
            jobs,
            deepest: AtomicUsize::new(0),
            nearest: AtomicUsize::new(0),
            late: AtomicUsize::new(0),
        }
    }

    /// `made` new jobs at `depth`, counted before the job that made them ends.
    pub(super) fn made(&self, depth: u16, made: usize) {
        if made > 0 {
            self.jobs[depth as usize].fetch_add(made, Ordering::Relaxed);
            self.deepest.fetch_max(depth as usize, Ordering::Relaxed);
        }
    }

    /// A job at `depth` finished: it walked its state and said what it made.
    pub(super) fn finished(&self, depth: u16) {
        self.jobs[depth as usize].fetch_sub(1, Ordering::Release);
    }

    /// A state first found at depth `first` was found again at depth `now`.
    pub(super) fn found_again(&self, first: u16, now: u16) {
        if first > now {
            self.late
                .fetch_max((first - now) as usize, Ordering::Release);
        }
    }

    /// How many moves from a start the walk has covered: every state that
    /// near has been walked.
    ///
    /// Take a state not yet walked, `k` moves away, and the first state on a
    /// shortest way to it whose job isn't finished, `i <= k` moves away. Each
    /// state before that one was walked, and found the next at one deeper
    /// than itself, or had it already at most `late` deeper still. So that
    /// state's depth is at most `i * (1 + late)`. It has a job left, so its
    /// depth is at least `nearest`, and `k >= nearest / (1 + late)`. On one
    /// thread `late` is 0, and this says `nearest - 1`.
    pub(super) fn covered(&self) -> usize {
        let nearest = self.nearest_job();
        if nearest == DEPTHS {
            return self.deepest.load(Ordering::Relaxed);
        }
        // After the jobs, so it holds what the finished ones found.
        let late = self.late.load(Ordering::Acquire);
        nearest.div_ceil(1 + late).saturating_sub(1)
    }

    /// How many moves the walk is working at: the depth of its nearest job.
    /// Threads can find a state late, so a state this near may not be walked
    /// yet, as [`Self::covered`] allows for; it says how far the walk has
    /// got, not what is proved.
    pub(super) fn reached(&self) -> usize {
        self.nearest_job().min(self.deepest.load(Ordering::Relaxed))
    }

    fn nearest_job(&self) -> usize {
        let mut nearest = self.nearest.load(Ordering::Relaxed);
        while nearest < DEPTHS && self.jobs[nearest].load(Ordering::Acquire) == 0 {
            nearest += 1;
        }
        self.nearest.store(nearest, Ordering::Relaxed);
        nearest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_walk_in_order_covers_what_is_nearer_than_its_next_job() {
        let levels = Levels::new(1);
        assert_eq!(levels.covered(), 0);
        levels.made(1, 2);
        levels.finished(0);
        levels.made(2, 1);
        levels.finished(1);
        assert_eq!(levels.covered(), 0, "a job at depth 1 is left");
        levels.finished(1);
        assert_eq!(levels.covered(), 1, "a job at depth 2 is left");
    }

    #[test]
    fn a_state_found_late_lowers_what_is_covered() {
        let levels = Levels::new(1);
        // A thread ran ahead: this state is found at depth 6, and is one
        // move from a start by a way another thread walks later.
        levels.made(1, 1);
        levels.finished(0);
        levels.made(6, 1);
        levels.finished(1);
        assert_eq!(levels.covered(), 5, "nothing says it was late yet");
        levels.found_again(6, 2);
        assert_eq!(levels.covered(), 1, "ceil(6 / 5) - 1 of the moves");
        assert_eq!(levels.reached(), 6, "the walk itself is that far along");
    }

    #[test]
    fn a_finished_walk_covers_all_it_found() {
        let levels = Levels::new(1);
        levels.made(1, 1);
        levels.finished(0);
        levels.finished(1);
        assert_eq!(levels.covered(), 1);
    }

    #[test]
    fn a_stopped_walk_leaves_its_jobs_unfinished() {
        let levels = Levels::new(1);
        levels.made(1, 1);
        levels.finished(0);
        levels.made(2, 3);
        assert_eq!(levels.covered(), 0, "the job at depth 1 never finished");
    }

    #[test]
    fn depths_past_the_last_count_as_the_last() {
        let last = DEPTHS as u16 - 1;
        assert_eq!(deeper(last - 1), last);
        assert_eq!(deeper(last), last);
    }
}
