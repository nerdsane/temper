//! Hot-swap protocol for transition tables.
//!
//! [`SwapController`] manages a versioned, thread-safe reference to the
//! current [`TransitionTable`]. A new table can be swapped in atomically
//! without restarting the actor or the process.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crate::table::TransitionTable;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Tracks transition table versions for hot-swapping.
pub struct SwapController {
    /// The currently active transition table.
    current: Arc<RwLock<TransitionTable>>,
    /// Monotonically increasing version counter.
    version: AtomicU64,
}

/// The result of a hot-swap attempt.
#[derive(Debug, Clone, PartialEq)]
pub enum SwapResult {
    /// Swap succeeded. Contains the old and new versions.
    Success { old_version: u64, new_version: u64 },
    /// Swap failed (e.g. lock poisoned).
    Failed(String),
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

impl SwapController {
    /// Create a new controller with the given initial table at version 1.
    pub fn new(table: TransitionTable) -> Self {
        SwapController {
            current: Arc::new(RwLock::new(table)),
            version: AtomicU64::new(1),
        }
    }

    /// Get a shared reference to the current transition table.
    pub fn current(&self) -> Arc<RwLock<TransitionTable>> {
        Arc::clone(&self.current)
    }

    /// Atomically swap the transition table to `new_table`.
    ///
    /// The version counter is incremented and the old table is replaced.
    pub fn swap(&self, new_table: TransitionTable) -> SwapResult {
        match self.current.write() {
            Ok(mut guard) => {
                let old_version = self.version.load(Ordering::SeqCst);
                *guard = new_table;
                let new_version = self.version.fetch_add(1, Ordering::SeqCst) + 1;
                SwapResult::Success {
                    old_version,
                    new_version,
                }
            }
            Err(e) => SwapResult::Failed(format!("RwLock poisoned: {e}")),
        }
    }

    /// Return the current version number.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::{Guard, TransitionRule, TransitionTable};

    fn dummy_table(name: &str) -> TransitionTable {
        let mut table = TransitionTable {
            entity_name: name.to_string(),
            states: vec!["A".into(), "B".into()],
            initial_state: "A".into(),
            rules: vec![TransitionRule {
                name: "GoB".into(),
                from_states: vec!["A".into()],
                to_state: Some("B".into()),
                guard: Guard::Always,
                effects: vec![],
            }],
            rule_index: Default::default(),
        };
        table.rebuild_index();
        table
    }

    #[test]
    fn new_controller_starts_at_version_1() {
        let ctrl = SwapController::new(dummy_table("v1"));
        assert_eq!(ctrl.version(), 1);
    }

    #[test]
    fn swap_increments_version() {
        let ctrl = SwapController::new(dummy_table("v1"));
        assert_eq!(ctrl.version(), 1);

        let result = ctrl.swap(dummy_table("v2"));
        assert_eq!(
            result,
            SwapResult::Success {
                old_version: 1,
                new_version: 2,
            }
        );
        assert_eq!(ctrl.version(), 2);
    }

    #[test]
    fn swap_replaces_table() {
        let ctrl = SwapController::new(dummy_table("v1"));

        ctrl.swap(dummy_table("v2"));

        let lock = ctrl.current();
        let table = lock.read().unwrap();
        assert_eq!(table.entity_name, "v2");
    }

    #[test]
    fn multiple_swaps() {
        let ctrl = SwapController::new(dummy_table("v1"));

        ctrl.swap(dummy_table("v2"));
        ctrl.swap(dummy_table("v3"));
        ctrl.swap(dummy_table("v4"));

        assert_eq!(ctrl.version(), 4);
        let lock = ctrl.current();
        let table = lock.read().unwrap();
        assert_eq!(table.entity_name, "v4");
    }
}
