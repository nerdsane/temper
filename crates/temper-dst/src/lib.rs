//! Shared helpers for Temper deterministic simulation tests.
//!
//! This crate is the DST **suite home**. Production binaries must not depend
//! on it. The engines stay in `temper-runtime`, `temper-store-sim`,
//! `temper-server` (`EntityActorHandler`), and `temper-verify` (L2 model sim).

#![allow(clippy::await_holding_lock)]

/// Register system-entity handlers on a [`SimActorSystem`].
pub mod actors;
/// `ServerState` builders and Order fixtures.
pub mod fixtures;
/// Production-path platform harness (install / dispatch / restart).
pub mod harness;
/// Platform invariants P1–P17 checked against a harness.
pub mod invariants;
/// Project / Tenant / Catalog / Collaborator / Version IOA sources.
pub mod system_specs;
/// Seeded install / dispatch / restart workload.
pub mod workload;

pub use actors as dst;
pub use fixtures::{
    CSDL_XML, ORDER_IOA, build_default_state, build_default_state_with_store,
    build_single_tenant_state, build_single_tenant_state_with_store, build_two_tenant_state,
    dispatch,
};
pub use harness as platform_harness;
pub use invariants as platform_invariants;
pub use system_specs as specs;
pub use workload as workload_gen;
