//! Dead-letter queue for permanently failed webhook deliveries.
//!
//! When all retry attempts are exhausted, the failed event is stored in the DLQ
//! for later inspection, manual replay, or alerting.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use super::types::DeadLetterEntry;

/// Dead-letter queue trait for permanently failed deliveries.
///
/// Uses boxed futures for dyn-compatibility so callers can use
/// `Arc<dyn DeadLetterQueue>`.
pub trait DeadLetterQueue: Send + Sync {
    /// Enqueue a failed delivery.
    fn enqueue(&self, entry: DeadLetterEntry) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;

    /// Peek at all entries (for inspection/alerting).
    fn list(&self) -> Pin<Box<dyn Future<Output = Vec<DeadLetterEntry>> + Send + '_>>;

    /// Remove an entry by index (after manual replay or acknowledgment).
    fn remove(
        &self,
        index: usize,
    ) -> Pin<Box<dyn Future<Output = Option<DeadLetterEntry>> + Send + '_>>;

    /// Number of entries in the DLQ.
    fn len(&self) -> Pin<Box<dyn Future<Output = usize> + Send + '_>>;

    /// Returns `true` if the DLQ contains no entries.
    fn is_empty(&self) -> Pin<Box<dyn Future<Output = bool> + Send + '_>>;
}

/// In-memory dead-letter queue for testing and single-node deployments.
#[derive(Clone)]
pub struct InMemoryDeadLetterQueue {
    entries: Arc<RwLock<Vec<DeadLetterEntry>>>,
}

impl InMemoryDeadLetterQueue {
    /// Create a new empty DLQ.
    pub fn new() -> Self {
        Self {
            entries: Arc::new(RwLock::new(Vec::new())),
        }
    }
}

impl Default for InMemoryDeadLetterQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl DeadLetterQueue for InMemoryDeadLetterQueue {
    fn enqueue(&self, entry: DeadLetterEntry) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.entries
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push(entry);
        Box::pin(std::future::ready(()))
    }

    fn list(&self) -> Pin<Box<dyn Future<Output = Vec<DeadLetterEntry>> + Send + '_>> {
        let result = self
            .entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        Box::pin(std::future::ready(result))
    }

    fn remove(
        &self,
        index: usize,
    ) -> Pin<Box<dyn Future<Output = Option<DeadLetterEntry>> + Send + '_>> {
        let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
        let result = if index < entries.len() {
            Some(entries.remove(index))
        } else {
            None
        };
        Box::pin(std::future::ready(result))
    }

    fn len(&self) -> Pin<Box<dyn Future<Output = usize> + Send + '_>> {
        let result = self
            .entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .len();
        Box::pin(std::future::ready(result))
    }

    fn is_empty(&self) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        let result = self
            .entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty();
        Box::pin(std::future::ready(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;

    use crate::integration::types::IntegrationEvent;

    fn test_entry(name: &str) -> DeadLetterEntry {
        DeadLetterEntry {
            integration_name: name.to_string(),
            event: IntegrationEvent {
                tenant: "test".to_string(),
                entity_type: "Order".to_string(),
                entity_id: "order-1".to_string(),
                event_name: "SubmitOrder".to_string(),
                from_status: "Draft".to_string(),
                to_status: "Submitted".to_string(),
                params: json!({}),
                timestamp: Utc::now(),
            },
            error: "connection refused".to_string(),
            attempts: 4,
            failed_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn test_enqueue_and_list() {
        let dlq = InMemoryDeadLetterQueue::new();
        assert_eq!(dlq.len().await, 0);

        dlq.enqueue(test_entry("hook_a")).await;
        dlq.enqueue(test_entry("hook_b")).await;

        let entries = dlq.list().await;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].integration_name, "hook_a");
        assert_eq!(entries[1].integration_name, "hook_b");
    }

    #[tokio::test]
    async fn test_remove_entry() {
        let dlq = InMemoryDeadLetterQueue::new();
        dlq.enqueue(test_entry("hook_a")).await;
        dlq.enqueue(test_entry("hook_b")).await;

        let removed = dlq.remove(0).await;
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().integration_name, "hook_a");

        assert_eq!(dlq.len().await, 1);
        let remaining = dlq.list().await;
        assert_eq!(remaining[0].integration_name, "hook_b");
    }

    #[tokio::test]
    async fn test_remove_out_of_bounds() {
        let dlq = InMemoryDeadLetterQueue::new();
        assert!(dlq.remove(0).await.is_none());
        assert!(dlq.remove(99).await.is_none());
    }
}
