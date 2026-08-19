use std::sync::{Arc, RwLock};

use temper_jit::table::TransitionTable;

/// Project system-entity IOA.
pub const PROJECT_IOA: &str = include_str!("../../temper-platform/src/specs/Project.ioa.toml");
/// Tenant system-entity IOA.
pub const TENANT_IOA: &str = include_str!("../../temper-platform/src/specs/Tenant.ioa.toml");
/// CatalogEntry system-entity IOA.
pub const CATALOG_ENTRY_IOA: &str =
    include_str!("../../temper-platform/src/specs/CatalogEntry.ioa.toml");
/// Collaborator system-entity IOA.
pub const COLLABORATOR_IOA: &str =
    include_str!("../../temper-platform/src/specs/Collaborator.ioa.toml");
/// Version system-entity IOA.
pub const VERSION_IOA: &str = include_str!("../../temper-platform/src/specs/Version.ioa.toml");
/// CSDL for platform system entities.
pub const SYSTEM_MODEL_CSDL_XML: &str =
    include_str!("../../temper-platform/src/specs/model.csdl.xml");

/// Parse the Project IOA into a shared transition table.
pub fn project_table_arc() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(PROJECT_IOA))
}

/// Parse the Tenant IOA into a shared transition table.
pub fn tenant_table_arc() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(TENANT_IOA))
}

/// Parse the CatalogEntry IOA into a shared transition table.
pub fn catalog_table_arc() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(CATALOG_ENTRY_IOA))
}

/// Parse the Collaborator IOA into a shared transition table.
pub fn collaborator_table_arc() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(COLLABORATOR_IOA))
}

/// Parse the Version IOA into a shared transition table.
pub fn version_table_arc() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(VERSION_IOA))
}

/// Parse the Project IOA into a lockable transition table.
pub fn project_table_rw() -> Arc<RwLock<TransitionTable>> {
    Arc::new(RwLock::new(TransitionTable::from_ioa_source(PROJECT_IOA)))
}

/// Parse the Tenant IOA into a lockable transition table.
pub fn tenant_table_rw() -> Arc<RwLock<TransitionTable>> {
    Arc::new(RwLock::new(TransitionTable::from_ioa_source(TENANT_IOA)))
}

/// Parse the CatalogEntry IOA into a lockable transition table.
pub fn catalog_table_rw() -> Arc<RwLock<TransitionTable>> {
    Arc::new(RwLock::new(TransitionTable::from_ioa_source(
        CATALOG_ENTRY_IOA,
    )))
}

/// Parse the Collaborator IOA into a lockable transition table.
pub fn collaborator_table_rw() -> Arc<RwLock<TransitionTable>> {
    Arc::new(RwLock::new(TransitionTable::from_ioa_source(
        COLLABORATOR_IOA,
    )))
}

/// Parse the Version IOA into a lockable transition table.
pub fn version_table_rw() -> Arc<RwLock<TransitionTable>> {
    Arc::new(RwLock::new(TransitionTable::from_ioa_source(VERSION_IOA)))
}
