use axum::Router;
use temper_platform::bootstrap::bootstrap_system_tenant;
use temper_platform::router::build_platform_router;
use temper_platform::state::PlatformState;

pub fn bootstrapped_state() -> PlatformState {
    let state = PlatformState::new(None);
    bootstrap_system_tenant(&state);
    state
}

pub fn bootstrapped_router() -> Router {
    build_platform_router(bootstrapped_state())
}
