//! Code generation from Temper specifications.
//!
//! Transforms CSDL entity models and IOA behavioral specs into Rust types.

mod entity;
mod generator;
mod messages;
mod state_machine;

pub use generator::{CodegenError, GeneratedModule, generate_entity_module};
