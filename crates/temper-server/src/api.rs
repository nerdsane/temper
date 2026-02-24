//! Management API routes (mutations).
//!
//! These endpoints handle spec loading, WASM module management, and evolution
//! decisions.  They are separated from the read-only `/observe` router so that
//! observe stays purely observational.

use axum::Router;
use axum::routing::post;

use crate::state::ServerState;

/// Build the management API router (mounted at /api).
///
/// Route structure:
/// - POST   /api/specs/load-dir                        → load specs from directory
/// - POST   /api/specs/load-inline                     → load specs from inline payload
/// - POST   /api/wasm/modules/{module_name}            → upload WASM module
/// - DELETE /api/wasm/modules/{module_name}             → delete WASM module
/// - POST   /api/evolution/records/{id}/decide          → developer decision on record
/// - POST   /api/evolution/trajectories/unmet           → report unmet user intent
/// - POST   /api/evolution/sentinel/check               → trigger sentinel health check
pub fn build_api_router() -> Router<ServerState> {
    Router::new()
        .route(
            "/specs/load-dir",
            post(crate::observe::specs::handle_load_dir),
        )
        .route(
            "/specs/load-inline",
            post(crate::observe::specs::handle_load_inline),
        )
        .route(
            "/wasm/modules/{module_name}",
            post(crate::observe::wasm::upload_wasm_module)
                .delete(crate::observe::wasm::delete_wasm_module),
        )
        .route(
            "/evolution/records/{id}/decide",
            post(crate::observe::evolution::handle_decide),
        )
        .route(
            "/evolution/trajectories/unmet",
            post(crate::observe::evolution::handle_unmet_intent),
        )
        .route(
            "/evolution/sentinel/check",
            post(crate::observe::evolution::handle_sentinel_check),
        )
}
