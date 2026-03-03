//! temper-evolution: The Evolution Engine.
//!
//! Manages the closed-loop feedback system from production observability
//! to specification changes. Records form an immutable chain:
//!
//! O-Record (Observation) → P-Record (Problem) → A-Record (Analysis)
//! → D-Record (Decision) → I-Record (Insight) → FR-Record (FeatureRequest)
//!
//! Each record links to its predecessor, creating a traceable proof chain
//! from anomaly detection to deployed change.

pub mod chain;
pub mod insight;
pub mod pg_store;
pub mod records;
pub mod store;

// Re-export primary types at crate root.
pub use chain::{ChainValidation, validate_chain};
pub use insight::{classify_insight, compute_priority_score, generate_digest};
pub use pg_store::{PgRecordStoreError, PostgresRecordStore};
pub use records::{
    AnalysisRecord, Decision, DecisionRecord, FeatureRequestDisposition, FeatureRequestRecord,
    InsightCategory, InsightRecord, InsightSignal, ObservationClass, ObservationRecord,
    PlatformGapCategory, ProblemRecord, RecordHeader, RecordId, RecordStatus, RecordType,
};
pub use store::RecordStore;
