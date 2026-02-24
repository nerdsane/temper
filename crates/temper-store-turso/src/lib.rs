//! # temper-store-turso
//!
//! Turso/libSQL storage backend for the Temper actor framework.
//!
//! This crate implements the [`EventStore`](temper_runtime::persistence::EventStore)
//! trait from `temper-runtime` using libSQL (Turso-compatible).

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
    pub created_at: &'a str,
}

pub use store::{
    TursoEventStore, TursoSpecRow, TursoTenantConstraintRow, TursoTrajectoryRow, TursoWasmModuleRow,
};
