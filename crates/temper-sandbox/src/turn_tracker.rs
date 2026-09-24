//! Per-execute work budgets with session-wide live-memory accounting.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use monty::{LimitedTracker, ResourceError, ResourceLimits, ResourceTracker};

#[derive(Debug)]
struct Budget {
    tracker: LimitedTracker,
    allocations: usize,
    allocation_limit: Option<usize>,
    duration: Option<Duration>,
}

/// The sandbox and its REPL share this handle. Only execute entry starts a turn;
/// external function suspension/resumption continues using the same budget.
#[derive(Clone, Debug)]
pub(crate) struct TurnTracker(Arc<Mutex<Budget>>, ResourceLimits);

impl TurnTracker {
    pub(crate) fn new(mut limits: ResourceLimits) -> Self {
        let original_limits = limits.clone();
        let allocation_limit = limits.max_allocations.take();
        let duration = limits.max_duration;
        Self(
            Arc::new(Mutex::new(Budget {
                tracker: LimitedTracker::new(limits),
                allocations: 0,
                allocation_limit,
                duration,
            })),
            original_limits,
        )
    }

    pub(crate) fn fresh(&self) -> Self {
        Self::new(self.1.clone())
    }

    fn budget(&self) -> MutexGuard<'_, Budget> {
        self.0.lock().expect("sandbox resource tracker poisoned")
    }

    pub(crate) fn begin_turn(&self) {
        let mut budget = self.budget();
        budget.allocations = 0;
        if let Some(duration) = budget.duration {
            budget.tracker.set_max_duration(duration);
        }
        // Do not replace LimitedTracker: its current_memory includes live globals.
    }
}

impl ResourceTracker for TurnTracker {
    fn on_allocate(&mut self, get_size: impl FnOnce() -> usize) -> Result<(), ResourceError> {
        let mut budget = self.budget();
        if let Some(limit) = budget.allocation_limit
            && budget.allocations >= limit
        {
            return Err(ResourceError::Allocation {
                limit,
                count: budget.allocations.saturating_add(1),
            });
        }
        budget.tracker.on_allocate(get_size)?;
        budget.allocations += 1;
        Ok(())
    }

    fn on_free(&mut self, get_size: impl FnOnce() -> usize) {
        self.budget().tracker.on_free(get_size);
    }

    fn check_time(&self) -> Result<(), ResourceError> {
        self.budget().tracker.check_time()
    }

    fn check_recursion_depth(&self, current_depth: usize) -> Result<(), ResourceError> {
        self.budget().tracker.check_recursion_depth(current_depth)
    }

    fn check_large_result(&self, estimated_bytes: usize) -> Result<(), ResourceError> {
        self.budget().tracker.check_large_result(estimated_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_turn_resets_work_but_keeps_live_memory() {
        let mut tracker = TurnTracker::new(ResourceLimits::new().max_allocations(2).max_memory(10));
        tracker.on_allocate(|| 4).unwrap();
        tracker.on_allocate(|| 4).unwrap();
        assert!(matches!(
            tracker.on_allocate(|| 1),
            Err(ResourceError::Allocation { .. })
        ));
        tracker.begin_turn();
        assert!(matches!(
            tracker.on_allocate(|| 3),
            Err(ResourceError::Memory { .. })
        ));
        tracker.on_allocate(|| 2).unwrap();
        tracker.on_free(|| 4);
        tracker.on_allocate(|| 4).unwrap();
        assert_eq!(tracker.budget().tracker.current_memory(), 10);
    }

    #[test]
    fn shared_resume_handle_does_not_refresh_allocation_budget() {
        let mut tracker = TurnTracker::new(ResourceLimits::new().max_allocations(1));
        let mut resumed = tracker.clone();
        tracker.on_allocate(|| 1).unwrap();
        assert!(matches!(
            resumed.on_allocate(|| 1),
            Err(ResourceError::Allocation { .. })
        ));
        tracker.begin_turn();
        resumed.on_allocate(|| 1).unwrap();
    }
}
