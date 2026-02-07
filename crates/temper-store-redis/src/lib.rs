//! temper-store-redis: Redis storage backend for Temper.
//!
//! Provides:
//! - Actor mailbox streams (Redis Streams for actor message queues)
//! - Actor placement cache (consistent hashing lookup)
//! - Distributed locks (for shard rebalancing)
//! - OData Function response cache (read-only, safe to cache)
//! - Entity state cache (read-through for hot entities)

pub mod keys;
pub mod mailbox;
pub mod placement;
pub mod cache;
pub mod error;

// Re-export primary types at crate root.
pub use error::RedisStoreError;
pub use mailbox::{MailboxEntry, MailboxStore, InMemoryMailbox};
pub use placement::{Placement, PlacementStore, InMemoryPlacement};
pub use cache::{CacheStore, InMemoryCache};
