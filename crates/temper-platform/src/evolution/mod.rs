//! Evolution pipeline integration.
//!
//! Captures unmet intents from the production agent, creates evolution
//! records, and routes approval requests to the developer chat.

pub mod feedback;

pub use feedback::{UnmetIntentCollector, UnmetIntent};
