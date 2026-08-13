//! Code generation from Temper specifications.
//!
//! Transforms CSDL entity models and IOA behavioral specs into Rust types.

mod entity;
mod generator;
mod messages;
mod module_sdk;
mod state_machine;

pub use generator::{CodegenError, GeneratedModule, generate_entity_module};
pub use module_sdk::{
    GeneratedModuleSdk, ModuleSdkCodegenError, PackagedModuleSdk, generate_module_sdk,
    package_generated_module_sdk,
};
