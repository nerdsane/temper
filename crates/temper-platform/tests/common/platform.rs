use std::collections::BTreeMap;

use axum::Router;
use temper_platform::bootstrap::{
    SYSTEM_TENANT, bootstrap_agent_specs, bootstrap_operator_credential, bootstrap_system_tenant,
};
use temper_platform::router::build_platform_router;
use temper_platform::state::PlatformState;

/// Tenant-scoped credential for the verified test operator.
pub const OPERATOR_KEY: &str = "test-operator-key";

pub fn bootstrapped_state() -> PlatformState {
    let state = PlatformState::new(None);
    bootstrap_system_tenant(&state, &BTreeMap::new());
    state
}

pub async fn bootstrapped_router() -> Router {
    let state = bootstrapped_state();
    bootstrap_agent_specs(&state, SYSTEM_TENANT, true, &BTreeMap::new());
    bootstrap_operator_credential(&state, OPERATOR_KEY, SYSTEM_TENANT).await;
    build_platform_router(state)
}
