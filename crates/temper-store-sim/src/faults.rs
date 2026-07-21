//! Deterministic one-shot fault controls for durable enumeration.

use super::*;

#[derive(Debug)]
pub(super) struct SimAppendDelay {
    pub(super) duration: Duration,
    pub(super) consumed: Option<tokio::sync::oneshot::Sender<()>>,
}

impl SimEventStore {
    /// Delay the next append and return a signal that resolves when it consumes the delay.
    ///
    /// This is the race-free synchronization primitive for fault campaigns that
    /// must admit another operation only after an append enters its controlled
    /// persistence window.
    pub fn inject_observed_append_delay(
        &self,
        persistence_id: &str,
        delay: Duration,
    ) -> tokio::sync::oneshot::Receiver<()> {
        let (consumed, observed) = tokio::sync::oneshot::channel();
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner
            .pending_append_delays
            .entry(persistence_id.to_string())
            .or_default()
            .push_back(SimAppendDelay {
                duration: delay,
                consumed: Some(consumed),
            });
        observed
    }

    /// Fail the next `count` typed entity-list operations, then recover.
    pub fn fail_next_typed_lists(&self, tenant: &str, entity_type: &str, count: usize) {
        let key = (tenant.to_string(), entity_type.to_string());
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        if count == 0 {
            inner.pending_typed_list_failures.remove(&key);
        } else {
            inner.pending_typed_list_failures.insert(key, count);
        }
    }

    /// Return the remaining deterministic typed-list failures.
    pub fn pending_typed_list_failures(&self, tenant: &str, entity_type: &str) -> usize {
        self.inner
            .lock()
            .expect("SimEventStore lock poisoned") // ci-ok: infallible lock
            .pending_typed_list_failures
            .get(&(tenant.to_string(), entity_type.to_string()))
            .copied()
            .unwrap_or(0)
    }

    /// Return the remaining deterministic journal-read failures for one actor.
    pub fn pending_read_failures(&self, persistence_id: &str) -> usize {
        self.inner
            .lock()
            .expect("SimEventStore lock poisoned") // ci-ok: infallible lock
            .pending_read_failures
            .get(persistence_id)
            .copied()
            .unwrap_or(0)
    }
}
