//! Hot-swap protocol for transition tables.
//!
//! [`SwapController`] manages a versioned, thread-safe reference to the
//! current [`TransitionTable`]. A new table can be swapped in atomically
//! without restarting the actor or the process.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::table::TransitionTable;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Infallible infrastructure callback for a newly published table version.
pub type SwapObserver = dyn Fn(u64) + Send + Sync;

struct SwapPublication {
    observer: Option<Arc<SwapObserver>>,
    last_version: u64,
}

/// Tracks transition table versions for hot-swapping.
pub struct SwapController {
    /// The currently active transition table.
    current: Arc<RwLock<TransitionTable>>,
    /// Monotonically increasing version counter.
    version: AtomicU64,
    /// Fixed single infrastructure observer and its monotonic publication fence.
    publication: Mutex<SwapPublication>,
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
            publication: Mutex::new(SwapPublication {
                observer: None,
                last_version: 1,
            }),
        }
    }

    /// Install the single observer invoked with each successfully published version.
    ///
    /// Callers should install the observer before publishing this controller
    /// to concurrent users. The callback runs after the table write lock is
    /// released, so it may safely read the newly installed table. The callback
    /// must not re-enter [`Self::swap`] and must not panic. A second install is
    /// rejected so public controller clones cannot replace server liveness
    /// notification or grow an unbounded callback collection.
    pub fn install_swap_observer(&self, observer: Arc<SwapObserver>) -> Result<(), String> {
        let mut publication = self
            .publication
            .lock()
            .map_err(|error| format!("swap publication lock poisoned: {error}"))?;
        if publication.observer.is_some() {
            return Err("swap observer already installed".to_string());
        }
        publication.observer = Some(observer);
        Ok(())
    }

    fn publish_version_locked(publication: &mut SwapPublication, version: u64) {
        if version <= publication.last_version {
            return;
        }
        publication.last_version = version;
        if let Some(observer) = publication.observer.as_ref() {
            observer(version);
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
        // Serialize replacement through notification. The table write lock is
        // released before the callback, while this publication lock prevents a
        // later swap from notifying first and regressing the version signal.
        let mut publication = match self.publication.lock() {
            Ok(publication) => publication,
            Err(error) => {
                return SwapResult::Failed(format!("swap publication lock poisoned: {error}"));
            }
        };
        let result = match self.current.write() {
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
        };
        if let SwapResult::Success { new_version, .. } = &result {
            Self::publish_version_locked(&mut publication, *new_version);
        }
        result
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
            state_timeouts: vec![],
            keys: vec![],
            vectors: vec![],
            rules: vec![TransitionRule {
                name: "GoB".into(),
                from_states: vec!["A".into()],
                to_state: Some("B".into()),
                guard: Guard::Always,
                effects: vec![],
            }],
            state_var_metadata: Default::default(),
            composite_actions: Default::default(),
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

    #[test]
    fn successful_swap_notifies_observer_after_replacement() {
        let ctrl = SwapController::new(dummy_table("v1"));
        let observed = Arc::new(AtomicU64::new(0));
        let observed_for_callback = observed.clone();
        ctrl.install_swap_observer(Arc::new(move |version| {
            observed_for_callback.store(version, Ordering::SeqCst);
        }))
        .expect("fresh observer lock");
        let second_observed = Arc::new(AtomicU64::new(0));
        let second_for_callback = second_observed.clone();
        let second_install = ctrl.install_swap_observer(Arc::new(move |version| {
            second_for_callback.store(version, Ordering::SeqCst);
        }));
        assert_eq!(
            second_install,
            Err("swap observer already installed".to_string())
        );

        assert!(matches!(
            ctrl.swap(dummy_table("v2")),
            SwapResult::Success { new_version: 2, .. }
        ));
        assert_eq!(observed.load(Ordering::SeqCst), 2);
        assert_eq!(second_observed.load(Ordering::SeqCst), 0);
        assert_eq!(
            ctrl.current().read().expect("table lock").entity_name,
            "v2",
            "the observer runs only after the replacement is visible"
        );
    }

    #[test]
    fn stale_publication_interleaving_cannot_regress_observer() {
        let ctrl = SwapController::new(dummy_table("v1"));
        let observed = Arc::new(AtomicU64::new(0));
        let observed_for_callback = observed.clone();
        ctrl.install_swap_observer(Arc::new(move |version| {
            observed_for_callback.store(version, Ordering::SeqCst);
        }))
        .expect("fresh observer lock");

        let mut publication = ctrl.publication.lock().expect("publication lock");
        SwapController::publish_version_locked(&mut publication, 3);
        SwapController::publish_version_locked(&mut publication, 2);

        assert_eq!(
            observed.load(Ordering::SeqCst),
            3,
            "a delayed older publication cannot regress the current signal"
        );
        assert_eq!(publication.last_version, 3);
    }
}
