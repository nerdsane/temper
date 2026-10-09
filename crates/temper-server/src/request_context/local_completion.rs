//! Typed local execution evidence, shared only by logically joined work.
use crate::idempotency::OperationReceipt;
use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, Ordering},
};

/// Records uncertainty at execution, join, timeout and required-cleanup
/// boundaries. Ordinary returned errors do not imply interrupted work. This
/// proves local completion only, never absence of remote side effects.
#[derive(Clone, Default)]
pub struct LocalCompletionEvidence {
    unknown: Arc<AtomicBool>,
    operation: Weak<OperationReceipt>,
}

impl std::fmt::Debug for LocalCompletionEvidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalCompletionEvidence")
            .field("unknown", &self.is_unknown())
            .field(
                "attempt",
                &self.operation.upgrade().map(|operation| operation.attempt),
            )
            .finish()
    }
}

impl LocalCompletionEvidence {
    /// Observe a known local dispatch wait; normal return does not clear prior uncertainty.
    pub(crate) fn observe_wait(&self) -> LocalCompletionWait {
        LocalCompletionWait {
            completion: self.clone(),
            joined: false,
        }
    }
    pub(crate) fn mark_unknown(&self) {
        self.unknown.store(true, Ordering::Release);
    }
    pub(crate) fn is_unknown(&self) -> bool {
        self.unknown.load(Ordering::Acquire)
    }
    pub(crate) fn for_operation(operation: &Arc<OperationReceipt>) -> Self {
        Self {
            unknown: Arc::new(AtomicBool::new(false)),
            operation: Arc::downgrade(operation),
        }
    }
    pub(crate) fn owned_result_unknown(&self) -> bool {
        self.is_unknown() && self.operation.upgrade().is_some()
    }
    pub(crate) fn require_known_result(&self) -> Result<(), UnknownOwnedCompletion> {
        if self.owned_result_unknown() {
            Err(UnknownOwnedCompletion)
        } else {
            Ok(())
        }
    }
    pub(crate) fn operation(&self) -> Option<Arc<OperationReceipt>> {
        self.operation.upgrade()
    }
}

/// A raw integration result cannot certify the outcome of unjoined owned work.
#[derive(Debug, thiserror::Error)]
#[error(
    "owned local completion is unknown; result-derived callbacks and compensation are disabled"
)]
pub(crate) struct UnknownOwnedCompletion;

/// Cancellation of a known dispatch wait does not prove its work ended.
pub(crate) struct LocalCompletionWait {
    completion: LocalCompletionEvidence,
    joined: bool,
}
impl LocalCompletionWait {
    /// The handler/dispatch wait returned; any inherited Unknown stays set.
    pub(crate) fn joined(mut self) {
        self.joined = true;
    }
}
impl Drop for LocalCompletionWait {
    fn drop(&mut self) {
        if !self.joined {
            self.completion.mark_unknown();
        }
    }
}
