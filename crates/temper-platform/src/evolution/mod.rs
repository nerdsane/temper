//! Evolution pipeline integration.
//!
//! - [`feedback`]: Captures unmet intents, creates O-Records and I-Records
//! - [`agents`]: Claude-powered O→P and P→A transformation agents

pub mod agents;
pub mod feedback;

pub use agents::{AnalysisAgent, ObservationAgent};
pub use feedback::{UnmetIntentCollector, UnmetIntent};
