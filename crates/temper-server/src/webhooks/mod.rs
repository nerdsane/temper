//! Webhook dispatch (outbound) and receiver (inbound) for entity transitions.

pub mod dispatcher;
pub mod receiver;

pub use dispatcher::{WebhookConfig, WebhookDispatcher};
pub use receiver::handle_webhook;
