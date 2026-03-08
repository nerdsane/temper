//! # temper-store-turso
//!
//! Turso/libSQL storage backend for the Temper actor framework.
//!
//! This crate implements the [`EventStore`](temper_runtime::persistence::EventStore)
//! trait from `temper-runtime` using libSQL (Turso-compatible).

pub mod router;
pub mod schema;
pub mod store;

#[derive(Clone, Copy, Debug)]
pub struct TursoSpecVerificationUpdate<'a> {
    pub status: &'a str,
    pub verified: bool,
    pub levels_passed: Option<i32>,
    pub levels_total: Option<i32>,
    pub verification_result_json: Option<&'a str>,
}

#[derive(Clone, Copy, Debug)]
pub struct TursoTrajectoryInsert<'a> {
    pub tenant: &'a str,
    pub entity_type: &'a str,
    pub entity_id: &'a str,
    pub action: &'a str,
    pub success: bool,
    pub from_status: Option<&'a str>,
    pub to_status: Option<&'a str>,
    pub error: Option<&'a str>,
    pub agent_id: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub authz_denied: Option<bool>,
    pub denied_resource: Option<&'a str>,
    pub denied_module: Option<&'a str>,
    pub source: Option<&'a str>,
    pub spec_governed: Option<bool>,
    pub created_at: &'a str,
}

#[derive(Clone, Copy, Debug)]
pub struct TursoWasmInvocationInsert<'a> {
    pub tenant: &'a str,
    pub entity_type: &'a str,
    pub entity_id: &'a str,
    pub module_name: &'a str,
    pub trigger_action: &'a str,
    pub callback_action: Option<&'a str>,
    pub success: bool,
    pub error: Option<&'a str>,
    pub duration_ms: u64,
    pub created_at: &'a str,
}

pub use router::{TenantRegistryRow, TenantStoreRouter, TenantUserRow};
pub use store::{
    AgentSummary, DesignTimeEventRow, EvolutionRecordRow, FeatureRequestRow, TursoEventStore,
    TursoSpecRow, TursoTenantConstraintRow, TursoTrajectoryRow, TursoWasmInvocationRow,
    TursoWasmModuleRow,
};
