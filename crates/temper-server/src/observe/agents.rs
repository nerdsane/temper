//! Agent audit endpoints for the observe API.
//!
//! Provides per-agent action history and summary statistics derived
//! from the trajectory log.

use axum::extract::{Path, Query, State};
use axum::response::Json;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
    let log = state.trajectory_log.read().unwrap(); // ci-ok: infallible lock
    let mut agents: BTreeMap<String, AgentSummary> = BTreeMap::new();

    for entry in log.entries() {
        // Skip entries without agent_id
        let agent_id = match entry.agent_id.as_deref() {
            Some(id) if !id.is_empty() => id,
            _ => continue,
        };

        // Apply tenant filter
        if let Some(ref tenant_filter) = params.tenant
            && entry.tenant != *tenant_filter
        {
            continue;
        }

        let summary = agents
            .entry(agent_id.to_string())
            .or_insert_with(|| AgentSummary {
                agent_id: agent_id.to_string(),
                total_actions: 0,
                success_count: 0,
                error_count: 0,
                denial_count: 0,
                success_rate: 0.0,
                last_active_at: None,
                entity_types: Vec::new(),
                tenants: Vec::new(),
            });

        summary.total_actions += 1;
        if entry.success {
            summary.success_count += 1;
        } else {
            summary.error_count += 1;
        }
        if entry.authz_denied == Some(true) {
            summary.denial_count += 1;
        }

        // Track latest timestamp
        if summary
            .last_active_at
            .as_ref()
            .is_none_or(|t| entry.timestamp > *t)
        {
            summary.last_active_at = Some(entry.timestamp.clone());
        }

        // Track entity types
        if !summary.entity_types.contains(&entry.entity_type) {
            summary.entity_types.push(entry.entity_type.clone());
        }

        // Track tenants
        if !summary.tenants.contains(&entry.tenant) {
            summary.tenants.push(entry.tenant.clone());
        }
    }

    // Compute success rates
    let mut agent_list: Vec<AgentSummary> = agents.into_values().collect();
    for agent in &mut agent_list {
        agent.success_rate = if agent.total_actions > 0 {
            agent.success_count as f64 / agent.total_actions as f64
        } else {
            0.0
        };
    }

    // Sort by last_active_at descending (most recent first)
    agent_list.sort_by(|a, b| b.last_active_at.cmp(&a.last_active_at));

    Json(serde_json::json!({
        "agents": agent_list,
        "total": agent_list.len(),
    }))
}

/// GET /observe/agents/{agent_id}/history -- full action timeline for one agent.
pub(crate) async fn get_agent_history(
    State(state): State<ServerState>,
    Path(agent_id): Path<String>,
    Query(params): Query<AgentHistoryParams>,
) -> Json<serde_json::Value> {
    let limit = params.limit.unwrap_or(100).min(500);
    let log = state.trajectory_log.read().unwrap(); // ci-ok: infallible lock

    let mut history: Vec<AgentHistoryEntry> = Vec::new();
    for entry in log.entries().iter().rev() {
        if history.len() >= limit {
            break;
        }

        let matches_agent = entry.agent_id.as_deref() == Some(&agent_id);
        if !matches_agent {
            continue;
        }

        // Apply filters
        if let Some(ref tenant_filter) = params.tenant
            && entry.tenant != *tenant_filter
        {
            continue;
        }
        if let Some(ref et_filter) = params.entity_type
            && entry.entity_type != *et_filter
        {
            continue;
        }

        history.push(AgentHistoryEntry {
            timestamp: entry.timestamp.clone(),
            tenant: entry.tenant.clone(),
            entity_type: entry.entity_type.clone(),
            entity_id: entry.entity_id.clone(),
            action: entry.action.clone(),
            success: entry.success,
            from_status: entry.from_status.clone(),
            to_status: entry.to_status.clone(),
            error: entry.error.clone(),
            authz_denied: entry.authz_denied.unwrap_or(false),
            denied_resource: entry.denied_resource.clone(),
        });
    }

    Json(serde_json::json!({
        "agent_id": agent_id,
        "history": history,
        "total": history.len(),
    }))
}
