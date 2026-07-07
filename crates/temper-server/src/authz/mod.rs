//! Authorization: Cedar policy enforcement and WASM host-function gating.

pub mod edge;
mod helpers;
pub mod policy_persistence;
pub mod wasm_gate;

pub use edge::{
    EdgeAuthenticatedPrincipal, materialize_authenticated_principal, strip_inbound_identity_headers,
};

#[allow(unused_imports)] // Used by observe/ handlers via crate::authz::observe_tenant_scope
pub(crate) use helpers::{
    DenialInput, GovernedMutationAuth, observe_tenant_scope, record_authz_denial,
    require_governed_mutation_auth, require_observe_auth, security_context_from_headers,
};
pub use policy_persistence::{load_and_activate_tenant_policies, persist_and_activate_policy};
pub use wasm_gate::{CedarWasmAuthzGate, PermissiveWasmAuthzGate};
