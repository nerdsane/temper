//! temper-evolution: The Evolution Engine.
//!
//! Manages the closed-loop feedback system from production observability
//! to specification changes. Records form an immutable chain:
//!
//! O-Record (Observation) → P-Record (Problem) → A-Record (Analysis)
//! → D-Record (Decision) → I-Record (Insight)
//!
//! Each record links to its predecessor, creating a traceable proof chain
//! from anomaly detection to deployed change.

pub mod records;
pub mod store;
pub mod chain;
pub mod insight;

// Re-export primary types at crate root.
pub use records::{
    RecordHeader, RecordType, RecordStatus, RecordId,
    ObservationRecord, ObservationClass, ProblemRecord, AnalysisRecord, DecisionRecord, InsightRecord,
    Decision, InsightCategory, InsightSignal,
};
pub use store::RecordStore;
pub use chain::{validate_chain, ChainValidation};
pub use insight::{compute_priority_score, classify_insight, generate_digest};
