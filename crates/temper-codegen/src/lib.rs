//! temper-codegen: Code generation from Temper specifications.
//!
//! Transforms CSDL entity models and TLA+ behavioral specs into Rust types:
//! - Entity state structs (from CSDL properties)
//! - Message enums (from CSDL actions/functions)
//! - State machine enums + transition tables (from TLA+ specs)
//! - Actor trait implementations

mod entity;
mod messages;
mod state_machine;
mod generator;

pub use generator::{generate_entity_module, GeneratedModule, CodegenError};
