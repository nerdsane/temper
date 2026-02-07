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
