//! Integration engine: outbox-pattern event-driven integrations.
//!
//! Integrations are declared in IOA specs via `[[integration]]` sections and
//! dispatched asynchronously after state transitions. The state machine remains
//! pure and deterministically verifiable — external calls happen out-of-band.
//!
//! Permanently failed deliveries are routed to a dead-letter queue for
//! later inspection or manual replay.

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
