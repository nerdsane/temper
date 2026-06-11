//! temper-store-redis: Redis event-journal backend for Temper.
//!
//! Provides the Redis-backed [`event_store::RedisEventStore`] used as an
//! event journal by the server's storage stack. Speculative mailbox,
//! placement, and cache modules were removed once nothing consumed them;
//! reintroduce them only alongside a real consumer.

pub mod error;
pub mod event_store;
pub mod keys;

// Re-export primary types at crate root.
pub use error::RedisStoreError;
pub use event_store::RedisEventStore;
