//! Platform integration suites share one executable without changing their inputs.
//!
//! Keep the suite sources beside the production crate so source-relative spec
//! fixtures retain their original paths. Each original suite is a named module,
//! allowing targeted runs with a `suite_name::` test filter.

#[path = "../../temper-platform/tests/common/mod.rs"]
mod common;

#[path = "../../temper-platform/tests/compile_first_e2e.rs"]
mod compile_first_e2e;
#[path = "../../temper-platform/tests/identity_e2e.rs"]
mod identity_e2e;
#[path = "../../temper-platform/tests/identity_expiry_e2e.rs"]
mod identity_expiry_e2e;
#[path = "../../temper-platform/tests/integration_engine.rs"]
mod integration_engine;
#[path = "../../temper-platform/tests/operator_bootstrap_cedar.rs"]
mod operator_bootstrap_cedar;
#[path = "../../temper-platform/tests/platform_e2e_dst.rs"]
mod platform_e2e_dst;
#[path = "../../temper-platform/tests/system_entity_actors.rs"]
mod system_entity_actors;
#[path = "../../temper-platform/tests/system_entity_dst.rs"]
mod system_entity_dst;
#[path = "../../temper-platform/tests/trusted_issuer_resolve.rs"]
mod trusted_issuer_resolve;
