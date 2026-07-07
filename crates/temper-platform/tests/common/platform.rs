use std::collections::BTreeMap;

use axum::Router;
use temper_platform::bootstrap::bootstrap_system_tenant;
use temper_platform::router::build_platform_router;
use temper_platform::state::PlatformState;

/// Global API key for the test operator. A request authenticates as the
/// operator (Admin) by sending `Authorization: Bearer {OPERATOR_KEY}` — client
/// `x-temper-principal-*` headers are stripped at the edge (ADR-0157).
pub const OPERATOR_KEY: &str = "test-operator-key";

pub fn bootstrapped_state() -> PlatformState {
    let mut state = PlatformState::new(None);
    state.api_token = Some(OPERATOR_KEY.to_string());
    bootstrap_system_tenant(&state, &BTreeMap::new());
    state
}

pub fn bootstrapped_router() -> Router {
    build_platform_router(bootstrapped_state())
}
