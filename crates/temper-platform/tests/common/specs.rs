use std::sync::{Arc, RwLock};

use temper_jit::table::TransitionTable;

pub const PROJECT_IOA: &str = include_str!("../../src/specs/Project.ioa.toml");
pub const TENANT_IOA: &str = include_str!("../../src/specs/Tenant.ioa.toml");
pub const CATALOG_ENTRY_IOA: &str = include_str!("../../src/specs/CatalogEntry.ioa.toml");
pub const COLLABORATOR_IOA: &str = include_str!("../../src/specs/Collaborator.ioa.toml");
pub const VERSION_IOA: &str = include_str!("../../src/specs/Version.ioa.toml");
pub const SYSTEM_MODEL_CSDL_XML: &str = include_str!("../../src/specs/model.csdl.xml");

pub fn project_table_arc() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(PROJECT_IOA))
}

pub fn tenant_table_arc() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(TENANT_IOA))
}

pub fn catalog_table_arc() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(CATALOG_ENTRY_IOA))
}

pub fn collaborator_table_arc() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(COLLABORATOR_IOA))
}

pub fn version_table_arc() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(VERSION_IOA))
}

pub fn project_table_rw() -> Arc<RwLock<TransitionTable>> {
    Arc::new(RwLock::new(TransitionTable::from_ioa_source(PROJECT_IOA)))
}

pub fn tenant_table_rw() -> Arc<RwLock<TransitionTable>> {
    Arc::new(RwLock::new(TransitionTable::from_ioa_source(TENANT_IOA)))
}

pub fn catalog_table_rw() -> Arc<RwLock<TransitionTable>> {
    Arc::new(RwLock::new(TransitionTable::from_ioa_source(
        CATALOG_ENTRY_IOA,
    )))
}

pub fn collaborator_table_rw() -> Arc<RwLock<TransitionTable>> {
    Arc::new(RwLock::new(TransitionTable::from_ioa_source(
        COLLABORATOR_IOA,
    )))
}

pub fn version_table_rw() -> Arc<RwLock<TransitionTable>> {
    Arc::new(RwLock::new(TransitionTable::from_ioa_source(VERSION_IOA)))
}
