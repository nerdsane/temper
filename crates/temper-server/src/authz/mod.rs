//! Authorization: Cedar policy enforcement and WASM host-function gating.

mod helpers;
pub mod policy_persistence;
pub mod wasm_gate;

#[allow(unused_imports)] // Used by observe/ handlers via crate::authz::observe_tenant_scope
pub(crate) use helpers::{
    DenialInput, observe_tenant_scope, record_authz_denial, require_observe_auth,
    security_context_from_headers,
};
pub use policy_persistence::{load_and_activate_tenant_policies, persist_and_activate_policy};
pub use wasm_gate::{CedarWasmAuthzGate, PermissiveWasmAuthzGate};
