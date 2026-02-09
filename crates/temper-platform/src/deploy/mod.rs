//! Verify-and-deploy pipeline for the platform.
//!
//! Orchestrates spec parsing, verification cascade, tenant registration,
//! and hot-swap deployment as a single atomic pipeline.

pub mod pipeline;

pub use pipeline::{DeployInput, DeployPipeline, DeployResult, EntitySpecSource};
