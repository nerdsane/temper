//! Verification, simulation, and workflow endpoints.
//!
//! Split into submodules for maintainability (was 800+ LOC).

mod cascade;
mod paths;
mod simulation;
mod status;
mod stream;
mod workflow;

pub(crate) use cascade::handle_run_verification;
pub(crate) use paths::handle_get_paths;
pub(crate) use simulation::handle_run_simulation;
pub(crate) use status::handle_verification_status;
pub(crate) use stream::handle_design_time_stream;
pub(crate) use workflow::handle_workflows;
