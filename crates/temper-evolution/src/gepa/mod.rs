//! GEPA: Guided Evolution of Pareto-optimal Artifacts
//!
//! Implements the core algorithm primitives for evolutionary optimization
//! of Temper skills (IOA specs). Based on arXiv:2507.19457.
//!
//! Architecture:
//! - Pure Rust primitives here (unit-testable, DST-compliant)
//! - WASM modules call these via host functions at runtime
//! - EvolutionRun IOA entity orchestrates the loop

pub mod candidate;
pub mod pareto;
pub mod reflective;
pub mod replay;
pub mod scoring;

pub use candidate::{Candidate, CandidateStatus};
pub use pareto::ParetoFrontier;
pub use reflective::ReflectiveTriplet;
pub use replay::ReplayResult;
pub use scoring::{ObjectiveScores, ScoringConfig};
