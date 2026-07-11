//! Authorization: Cedar policy enforcement and WASM host-function gating.

mod helpers;
pub mod policy_persistence;
pub mod wasm_gate;

#[allow(unused_imports)] // Used by observe/ handlers via crate::authz::observe_tenant_scope
pub(crate) use helpers::{
    DenialInput, GovernedMutationAuth, observe_tenant_scope, record_authz_denial,
    require_governed_mutation_auth, require_observe_auth, security_context_from_headers,
};
pub use policy_persistence::{
    DecisionPolicyInstall, DecisionPolicyReceipt, PolicyEntryUpsert, PolicyPublicationError,
    fail_closed_tenant_policies, install_decision_policy, load_and_activate_tenant_policies,
    publish_policy_snapshot, recover_policy_snapshot, refresh_policy_snapshot_if_stale,
    rollback_created_decision_policy, upsert_policy_entries, verify_active_policy_exactly_once,
};
pub use wasm_gate::{CedarWasmAuthzGate, PermissiveWasmAuthzGate};
