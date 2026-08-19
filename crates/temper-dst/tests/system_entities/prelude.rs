//! Shared imports for system-entity DST modules.

#![allow(unused_imports)]

pub use std::sync::Arc;

pub use temper_dst as common;
pub use temper_jit::table::TransitionTable;
pub use temper_runtime::scheduler::{FaultConfig, RunRecord, SimActorSystem, SimActorSystemConfig};
pub use temper_server::entity_actor::sim_handler::EntityActorHandler;

pub use common::dst::*;
pub use common::specs::*;

pub fn project_table() -> Arc<TransitionTable> {
    project_table_arc()
}

pub fn tenant_table() -> Arc<TransitionTable> {
    tenant_table_arc()
}

pub fn catalog_table() -> Arc<TransitionTable> {
    catalog_table_arc()
}

pub fn collaborator_table() -> Arc<TransitionTable> {
    collaborator_table_arc()
}

pub fn version_table() -> Arc<TransitionTable> {
    version_table_arc()
}
