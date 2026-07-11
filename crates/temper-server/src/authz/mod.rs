//! Authorization: Cedar policy enforcement and WASM host-function gating.

pub mod edge;
mod helpers;
pub mod policy_persistence;
pub mod wasm_gate;

pub use edge::{
    is_public_kernel_request, require_authenticated_request_context, strip_inbound_identity_headers,
};
pub use temper_authz::AuthenticatedRequestContext;

#[allow(unused_imports)] // Used by observe/ handlers via crate::authz::observe_tenant_scope
pub(crate) use helpers::{
    DenialInput, GovernedMutationAuth, observe_tenant_scope, record_authz_denial,
    require_authenticated_context, require_governed_mutation_auth, require_observe_auth,
    require_tenant_match,
};
#[cfg(feature = "observe")]
pub(crate) use helpers::{ResourceAuthorization, require_resource_authorization};
pub use policy_persistence::{
    load_and_activate_tenant_policies, persist_and_activate_policy, record_policy_change,
};
pub use wasm_gate::{CedarWasmAuthzGate, PermissiveWasmAuthzGate};
