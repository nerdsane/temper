//! temper-store-redis: Redis storage backend for Temper.
//!
//! Provides:
//! - Actor mailbox streams (Redis Streams for actor message queues)
//! - Actor placement cache (consistent hashing lookup)
//! - Distributed locks (for shard rebalancing)
//! - OData Function response cache (read-only, safe to cache)
//! - Entity state cache (read-through for hot entities)

pub mod cache;
pub mod error;
pub mod event_store;
pub mod keys;
pub mod mailbox;
pub mod placement;

// Re-export primary types at crate root.
pub use cache::{CacheStore, InMemoryCache, RedisCache};
pub use error::RedisStoreError;
pub use event_store::RedisEventStore;
pub use mailbox::{InMemoryMailbox, MailboxEntry, MailboxStore, RedisMailbox};
pub use placement::{InMemoryPlacement, Placement, PlacementStore, RedisPlacement};
