//! Redis store error types.

#[derive(Debug, thiserror::Error)]
pub enum RedisStoreError {
    #[error("redis connection error: {0}")]
    Connection(String),

    #[error("redis command error: {0}")]
    Command(String),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("lock acquisition failed: {0}")]
    LockFailed(String),

    #[error("mailbox error: {0}")]
    Mailbox(String),

    #[error("cache miss for key: {0}")]
    CacheMiss(String),
}
