//! Agent audit endpoints for the observe API.
//!
//! Provides per-agent action history and summary statistics derived
//! from Turso (single source of truth).

use axum::extract::{Path, Query, State};
use axum::response::Json;
use serde::{Deserialize, Serialize};

use crate::state::ServerState;

/// Summary of a single agent's activity.
#[derive(Debug, Serialize)]
pub struct AgentSummary {
    /// Agent identifier.
    pub agent_id: String,
    /// Total actions performed.
    pub total_actions: usize,
    /// Successful actions.
    pub success_count: usize,
    /// Failed actions (guard rejections, errors).
    pub error_count: usize,
    /// Authorization denial count.
    pub denial_count: usize,
    /// Success rate (0.0 - 1.0).
    pub success_rate: f64,
    /// ISO-8601 timestamp of last activity.
    pub last_active_at: Option<String>,
    /// Entity types this agent interacted with.
    pub entity_types: Vec<String>,
    /// Tenants this agent operated in.
    pub tenants: Vec<String>,
}

/// A single entry in an agent's action history.
#[derive(Debug, Serialize)]
pub struct AgentHistoryEntry {
    /// ISO-8601 timestamp.
    pub timestamp: String,
    /// Tenant.
    pub tenant: String,
    /// Entity type targeted.
    pub entity_type: String,
    /// Entity ID targeted.
    pub entity_id: String,
    /// Action name.
    pub action: String,
    /// Whether the action succeeded.
    pub success: bool,
    /// From status (if known).
    pub from_status: Option<String>,
    /// To status (if known).
    pub to_status: Option<String>,
    /// Error message (if failed).
    pub error: Option<String>,
    /// Whether this was an authorization denial.
    pub authz_denied: bool,
    /// Denied resource (if authz denial).
    pub denied_resource: Option<String>,
}

/// Query parameters for listing agents.
#[derive(Deserialize)]
pub struct ListAgentsParams {
    /// Filter by tenant.
    pub tenant: Option<String>,
}

/// Query parameters for agent history.
#[derive(Deserialize)]
pub struct AgentHistoryParams {
    /// Filter by tenant.
    pub tenant: Option<String>,
    /// Filter by entity type.
    pub entity_type: Option<String>,
    /// Maximum entries to return (default: 100).
    pub limit: Option<usize>,
}

/// GET /observe/agents -- list agents with action/denial counts.
pub(crate) async fn list_agents(
    State(state): State<ServerState>,
    Query(params): Query<ListAgentsParams>,
) -> Json<serde_json::Value> {
    // Query Turso directly (single source of truth).
    if let Some(turso) = state.turso_opt() {
        match turso.query_agent_summaries(params.tenant.as_deref()).await {
            Ok(summaries) => {
                let agents: Vec<AgentSummary> = summaries
                    .into_iter()
                    .map(|s| AgentSummary {
                        agent_id: s.agent_id,
                        total_actions: s.total_actions as usize,
                        success_count: s.success_count as usize,
                        error_count: s.error_count as usize,
                        denial_count: s.denial_count as usize,
                        success_rate: s.success_rate,
                        last_active_at: Some(s.last_active_at),
                        entity_types: Vec::new(),
                        tenants: Vec::new(),
                    })
                    .collect();
                let total = agents.len();
                return Json(serde_json::json!({
                    "agents": agents,
                    "total": total,
                }));
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to query agent summaries from Turso");
            }
        }
    }

    // Fallback: empty response when no Turso configured.
    Json(serde_json::json!({
        "agents": [],
        "total": 0,
    }))
}

/// GET /observe/agents/{agent_id}/history -- full action timeline for one agent.
pub(crate) async fn get_agent_history(
    State(state): State<ServerState>,
    Path(agent_id): Path<String>,
    Query(params): Query<AgentHistoryParams>,
) -> Json<serde_json::Value> {
    let limit = params.limit.unwrap_or(100).min(500);

    // Query Turso directly (single source of truth).
    if let Some(turso) = state.turso_opt() {
        match turso
            .query_trajectories_by_agent(
                &agent_id,
                params.tenant.as_deref(),
                params.entity_type.as_deref(),
                limit as i64,
            )
            .await
        {
            Ok(rows) => {
                let history: Vec<AgentHistoryEntry> = rows
                    .into_iter()
                    .map(|r| AgentHistoryEntry {
                        timestamp: r.created_at,
                        tenant: r.tenant,
                        entity_type: r.entity_type,
                        entity_id: r.entity_id,
                        action: r.action,
                        success: r.success,
                        from_status: r.from_status,
                        to_status: r.to_status,
                        error: r.error,
                        authz_denied: r.authz_denied.unwrap_or(false),
                        denied_resource: r.denied_resource,
                    })
                    .collect();
                let total = history.len();
                return Json(serde_json::json!({
                    "agent_id": agent_id,
                    "history": history,
                    "total": total,
                }));
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to query agent history from Turso");
            }
        }
    }

    // Fallback: empty response when no Turso configured.
    Json(serde_json::json!({
        "agent_id": agent_id,
        "history": [],
        "total": 0,
    }))
}
