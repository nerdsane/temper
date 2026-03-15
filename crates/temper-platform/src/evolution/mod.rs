//! Evolution pipeline integration.
//!
//! - [`feedback`]: Captures unmet intents, creates O-Records and I-Records
//! - [`agents`]: Claude-powered O→P and P→A transformation agents

pub mod agents;
pub mod feedback;

pub use agents::{AnalysisAgent, ObservationAgent};
pub use feedback::{UnmetIntent, UnmetIntentCollector};

pub(crate) fn trace_record_creation(
    record_type: &str,
    record_id: &str,
    created_by: &str,
    derived_from: Option<&str>,
    tenant: Option<&str>,
) {
    tracing::info!(
        record_type,
        record_id,
        created_by,
        derived_from,
        tenant,
        "evolution.record.create"
    );
}
