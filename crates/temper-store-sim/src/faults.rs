//! Deterministic one-shot fault controls for durable enumeration.

use super::*;

impl SimEventStore {
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
