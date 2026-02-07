//! Actor mailbox backed by Redis Streams.
//!
//! Each actor has a dedicated Redis Stream for its mailbox.
//! Messages are appended with XADD and consumed with XREAD.
//! This enables distributed actor messaging across nodes.

use serde::{Deserialize, Serialize};

use crate::keys;

/// A message envelope in the Redis mailbox stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxEntry {
    /// The message type (e.g., "SubmitOrder", "CancelOrder").
    pub msg_type: String,
    /// JSON-serialized message payload.
    pub payload: String,
    /// Sender actor ID (for reply routing).
    pub sender: Option<String>,
    /// Correlation ID for distributed tracing.
    pub correlation_id: String,
    /// Timestamp of when the message was sent.
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// Mailbox operations (trait for testability without Redis connection).
pub trait MailboxStore: Send + Sync + 'static {
    /// Send a message to an actor's mailbox.
    fn send(
        &self,
        actor_id: &str,
        entry: &MailboxEntry,
    ) -> impl std::future::Future<Output = Result<String, crate::error::RedisStoreError>> + Send;

    /// Receive the next message from an actor's mailbox.
    /// Returns None if no messages are available (non-blocking).
    fn receive(
        &self,
        actor_id: &str,
    ) -> impl std::future::Future<Output = Result<Option<MailboxEntry>, crate::error::RedisStoreError>> + Send;

    /// Get the current depth (pending message count) of a mailbox.
    fn depth(
        &self,
        actor_id: &str,
    ) -> impl std::future::Future<Output = Result<u64, crate::error::RedisStoreError>> + Send;
}

/// In-memory mailbox for testing (no Redis needed).
pub struct InMemoryMailbox {
    queues: std::sync::Arc<std::sync::RwLock<std::collections::HashMap<String, std::collections::VecDeque<MailboxEntry>>>>,
}

impl InMemoryMailbox {
    pub fn new() -> Self {
        Self {
            queues: std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
        }
    }
}

impl Default for InMemoryMailbox {
    fn default() -> Self {
        Self::new()
    }
}

impl MailboxStore for InMemoryMailbox {
    async fn send(
        &self,
        actor_id: &str,
        entry: &MailboxEntry,
    ) -> Result<String, crate::error::RedisStoreError> {
        let key = keys::mailbox_key(actor_id);
        let mut queues = self.queues.write().unwrap();
        queues.entry(key).or_default().push_back(entry.clone());
        Ok(uuid::Uuid::now_v7().to_string())
    }

    async fn receive(
        &self,
        actor_id: &str,
    ) -> Result<Option<MailboxEntry>, crate::error::RedisStoreError> {
        let key = keys::mailbox_key(actor_id);
        let mut queues = self.queues.write().unwrap();
        Ok(queues.get_mut(&key).and_then(|q| q.pop_front()))
    }

    async fn depth(
        &self,
        actor_id: &str,
    ) -> Result<u64, crate::error::RedisStoreError> {
        let key = keys::mailbox_key(actor_id);
        let queues = self.queues.read().unwrap();
        Ok(queues.get(&key).map_or(0, |q| q.len() as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_entry(msg_type: &str) -> MailboxEntry {
        MailboxEntry {
            msg_type: msg_type.to_string(),
            payload: r#"{"key":"value"}"#.to_string(),
            sender: Some("test-sender".to_string()),
            correlation_id: uuid::Uuid::now_v7().to_string(),
            timestamp: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn test_send_and_receive() {
        let mailbox = InMemoryMailbox::new();

        mailbox.send("actor-1", &test_entry("SubmitOrder")).await.unwrap();
        mailbox.send("actor-1", &test_entry("CancelOrder")).await.unwrap();

        let msg1 = mailbox.receive("actor-1").await.unwrap().unwrap();
        assert_eq!(msg1.msg_type, "SubmitOrder");

        let msg2 = mailbox.receive("actor-1").await.unwrap().unwrap();
        assert_eq!(msg2.msg_type, "CancelOrder");

        // Empty now
        let msg3 = mailbox.receive("actor-1").await.unwrap();
        assert!(msg3.is_none());
    }

    #[tokio::test]
    async fn test_mailbox_depth() {
        let mailbox = InMemoryMailbox::new();

        assert_eq!(mailbox.depth("actor-1").await.unwrap(), 0);

        mailbox.send("actor-1", &test_entry("A")).await.unwrap();
        mailbox.send("actor-1", &test_entry("B")).await.unwrap();
        assert_eq!(mailbox.depth("actor-1").await.unwrap(), 2);

        mailbox.receive("actor-1").await.unwrap();
        assert_eq!(mailbox.depth("actor-1").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn test_separate_actor_mailboxes() {
        let mailbox = InMemoryMailbox::new();

        mailbox.send("actor-1", &test_entry("A")).await.unwrap();
        mailbox.send("actor-2", &test_entry("B")).await.unwrap();

        let msg1 = mailbox.receive("actor-1").await.unwrap().unwrap();
        assert_eq!(msg1.msg_type, "A");

        let msg2 = mailbox.receive("actor-2").await.unwrap().unwrap();
        assert_eq!(msg2.msg_type, "B");
    }

    #[tokio::test]
    async fn test_fifo_ordering() {
        let mailbox = InMemoryMailbox::new();

        for i in 0..5 {
            mailbox.send("actor-1", &test_entry(&format!("msg-{i}"))).await.unwrap();
        }

        for i in 0..5 {
            let msg = mailbox.receive("actor-1").await.unwrap().unwrap();
            assert_eq!(msg.msg_type, format!("msg-{i}"));
        }
    }
}
