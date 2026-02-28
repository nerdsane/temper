//! Bounded invocation log for WASM integration calls.

use std::collections::VecDeque;

/// A single WASM invocation record.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WasmInvocationEntry {
    /// ISO-8601 timestamp (DST-safe: uses sim_now()).
    pub timestamp: String,
    /// Tenant that triggered the invocation.
    pub tenant: String,
    /// Entity type that produced the custom effect.
    pub entity_type: String,
    /// Entity ID that produced the custom effect.
    pub entity_id: String,
    /// WASM module name invoked.
    pub module_name: String,
    /// Action that triggered the integration.
    pub trigger_action: String,
    /// Callback action dispatched after invocation (if any).
    pub callback_action: Option<String>,
    /// Whether the invocation succeeded.
    pub success: bool,
    /// Error description (for failures).
    pub error: Option<String>,
    /// Invocation duration in milliseconds.
    pub duration_ms: u64,
    /// Whether this failure was due to an authorization denial.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authz_denied: Option<bool>,
}

/// Bounded, append-only WASM invocation log.
///
/// Uses `VecDeque` with a fixed capacity. When the log is full, the oldest
/// entry is evicted (ring-buffer semantics). Same pattern as `TrajectoryLog`.
pub struct WasmInvocationLog {
    /// The bounded deque of invocation entries.
    entries: VecDeque<WasmInvocationEntry>,
    /// Maximum capacity.
    capacity: usize,
}

impl WasmInvocationLog {
    /// Create a new invocation log with the given capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Append an entry, evicting the oldest if at capacity.
    pub fn push(&mut self, entry: WasmInvocationEntry) {
        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    /// Read-only access to all entries (oldest first).
    pub fn entries(&self) -> &VecDeque<WasmInvocationEntry> {
        &self.entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry(module: &str, success: bool) -> WasmInvocationEntry {
        WasmInvocationEntry {
            timestamp: "2025-01-01T00:00:00Z".to_string(),
            tenant: "default".to_string(),
            entity_type: "Order".to_string(),
            entity_id: "order-1".to_string(),
            module_name: module.to_string(),
            trigger_action: "Submit".to_string(),
            callback_action: Some("OnSubmitResult".to_string()),
            success,
            error: if success {
                None
            } else {
                Some("module error".to_string())
            },
            duration_ms: 42,
            authz_denied: None,
        }
    }

    #[test]
    fn push_and_read() {
        let mut log = WasmInvocationLog::new(10);
        log.push(sample_entry("mod-a", true));
        log.push(sample_entry("mod-b", false));
        assert_eq!(log.entries().len(), 2);
        assert_eq!(log.entries()[0].module_name, "mod-a");
        assert_eq!(log.entries()[1].module_name, "mod-b");
        assert!(log.entries()[0].success);
        assert!(!log.entries()[1].success);
    }

    #[test]
    fn evicts_oldest_at_capacity() {
        let mut log = WasmInvocationLog::new(3);
        log.push(sample_entry("mod-1", true));
        log.push(sample_entry("mod-2", true));
        log.push(sample_entry("mod-3", true));
        assert_eq!(log.entries().len(), 3);

        // Push a 4th — should evict mod-1
        log.push(sample_entry("mod-4", true));
        assert_eq!(log.entries().len(), 3);
        assert_eq!(log.entries()[0].module_name, "mod-2");
        assert_eq!(log.entries()[2].module_name, "mod-4");
    }

    #[test]
    fn empty_log() {
        let log = WasmInvocationLog::new(5);
        assert!(log.entries().is_empty());
    }
}
