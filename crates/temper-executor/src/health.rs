//! Health check HTTP endpoint for the executor.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anyhow::{Context, Result};
use axum::extract::State as AxumState;
use axum::response::Json;
use axum::routing::get;
use tracing::info;

/// Shared state for the health endpoint.
#[derive(Clone)]
pub struct HealthState {
    /// Unique executor identity.
    pub executor_id: String,
    /// Maximum number of concurrent agent runs.
    pub max_concurrent: usize,
    /// Number of currently active agents.
    pub active_agents: Arc<AtomicUsize>,
    /// Whether the executor is shutting down.
    pub shutting_down: Arc<AtomicBool>,
}

/// Health check endpoint handler.
pub async fn health_handler(AxumState(state): AxumState<HealthState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": if state.shutting_down.load(Ordering::Relaxed) { "draining" } else { "healthy" },
        "executor_id": state.executor_id,
        "active_agents": state.active_agents.load(Ordering::Relaxed),
        "max_concurrent": state.max_concurrent,
    }))
}

/// Run the health check HTTP server.
pub async fn run_health_server(port: u16, state: HealthState) -> Result<()> {
    let app = axum::Router::new()
        .route("/health", get(health_handler))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .context("Failed to bind health port")?;
    info!(port = port, "Health endpoint listening");
    axum::serve(listener, app)
        .await
        .context("Health server error")?;
    Ok(())
}
