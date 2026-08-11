//! Temper OTS - Open Trajectory Specification for Temper
//!
//! A DST-compatible (Deterministic Simulation Testing) implementation of the
//! Open Trajectory Specification for capturing agent decision traces. All types
//! use `BTreeMap` for deterministic iteration order and delegate ID/time
//! generation to `temper-runtime`'s `sim_uuid()` / `sim_now()`.
//!
//! # Features
//!
//! - **Core Models**: Complete type-safe OTS data structures
//! - **DST Compatible**: All types use deterministic collections and sim-aware constructors
//! - **Builder**: Incremental trajectory construction via [`TrajectoryBuilder`]
//! - **ATIF Export**: Harbor ATIF v1.7 interchange output via [`atif::to_atif`]

pub mod atif;
pub mod builder;
pub mod models;

// Re-exports for convenience
pub use atif::{ATIF_SCHEMA_VERSION, AtifTrajectory, to_atif};
pub use builder::TrajectoryBuilder;
pub use models::{
    DecisionType, EvaluatorType, MessageRole, OTSAnnotation, OTSChoice, OTSConsequence, OTSContext,
    OTSDecision, OTSEntity, OTSEvaluator, OTSMessage, OTSMessageContent, OTSMetadata, OTSResource,
    OTSSystemMessage, OTSTrajectory, OTSTurn, OTSUser, OutcomeType,
};
