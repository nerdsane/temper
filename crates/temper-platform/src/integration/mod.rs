//! Directly configured webhook integration engine.
//!
//! Callers construct an [`IntegrationRegistry`] from [`IntegrationConfig`] values
//! and explicitly submit [`IntegrationEvent`] values. Production entity actors do
//! not populate this registry from IOA declarations or feed transitions into it.
//! The queue and dead-letter queue are in memory, not a durable outbox; outbound
//! IOA webhooks are therefore rejected until journaled delivery is available.
//! Retry and dead-letter behavior remains available to direct API callers.

pub mod dead_letter;
pub mod engine;
pub mod registry;
pub mod types;
pub mod webhook;

pub use dead_letter::{DeadLetterQueue, InMemoryDeadLetterQueue};
pub use engine::IntegrationEngine;
pub use registry::IntegrationRegistry;
pub use types::{
    DeadLetterEntry, IntegrationConfig, IntegrationEvent, IntegrationResult, IntegrationStatus,
    RetryPolicy, WebhookConfig,
};
pub use webhook::WebhookDispatcher;
