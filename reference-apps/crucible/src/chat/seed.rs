//! Fixture helper that creates an Environment, a ManagedAgent, and a
//! Session so `crucible-chat send` / `crucible-chat respond` have
//! something to talk to.
//!
//! This is the `seed` subcommand. It is also called directly from
//! the integration test so the test does not have to repeat the POST
//! bodies. Every row uses the minimum set of fields the Crucible
//! field invariants require — the Phase 0 hard constraint is
//! respected by always choosing `Local` + `Unrestricted` + no MCP
//! servers + no package managers.

use crate::chat::temper_client::{
    AgentToolRow, EnvironmentRow, ManagedAgentRow, SessionRow, TemperClient,
};
use anyhow::{Context, Result};
use uuid::Uuid;

/// Defaults for the `seed` path. Every field is overridable so
/// tests can pin deterministic ids.
#[derive(Debug, Clone)]
pub struct SeedOptions {
    pub environment_id: Option<String>,
    pub agent_id: Option<String>,
    pub session_id: Option<String>,
    pub agent_name: String,
    pub system_prompt: String,
    pub model_id: String,
    pub model_speed: String,
    pub now: String,
}

impl Default for SeedOptions {
    fn default() -> Self {
        Self {
            environment_id: None,
            agent_id: None,
            session_id: None,
            agent_name: "crucible-chat-agent".to_string(),
            system_prompt:
                "You are a concise, helpful assistant running inside the Crucible reference app."
                    .to_string(),
            model_id: "claude-sonnet-4-6".to_string(),
            model_speed: "standard".to_string(),
            now: "2026-04-11T00:00:00Z".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SeedOutcome {
    pub environment_id: String,
    pub agent_id: String,
    pub session_id: String,
}

/// POST a fresh Environment + ManagedAgent + Session and return the
/// resulting ids. The three entities are created in parent-first
/// order because the Session cross-invariants (ADR-0044) reject a
/// Session whose parent ManagedAgent or Environment does not exist.
pub async fn seed(temper: &TemperClient, opts: SeedOptions) -> Result<SeedOutcome> {
    let env_id = opts
        .environment_id
        .clone()
        .unwrap_or_else(|| format!("env-chat-{}", short_uuid()));
    let agent_id = opts
        .agent_id
        .clone()
        .unwrap_or_else(|| format!("agt-chat-{}", short_uuid()));
    let session_id = opts
        .session_id
        .clone()
        .unwrap_or_else(|| format!("sess-chat-{}", short_uuid()));

    // ------------------------------------------------------------------
    // Environment: Local + Unrestricted — the minimum-cost combination.
    // ------------------------------------------------------------------
    let env_row = EnvironmentRow {
        id: env_id.clone(),
        name: "crucible-chat-env".to_string(),
        status: "Active".to_string(),
        config_type: "Local".to_string(),
        networking_type: "Unrestricted".to_string(),
        created_at: opts.now.clone(),
        updated_at: opts.now.clone(),
        description: Some("Phase 4 chat-loop environment".to_string()),
        allow_mcp_servers: None,
        allow_package_managers: None,
        metadata: None,
    };
    temper
        .create_environment(&env_row)
        .await
        .context("seeding Environment")?;

    // ------------------------------------------------------------------
    // ManagedAgent: the system prompt and model id live here, not in
    // SessionRow — same shape as Anthropic's upstream surface.
    // ------------------------------------------------------------------
    let agent_row = ManagedAgentRow {
        id: agent_id.clone(),
        name: opts.agent_name.clone(),
        model_id: opts.model_id.clone(),
        status: "Active".to_string(),
        version: 1,
        created_at: opts.now.clone(),
        updated_at: opts.now.clone(),
        description: Some("Phase 4 chat-loop agent".to_string()),
        system: Some(opts.system_prompt.clone()),
        model_speed: Some(opts.model_speed.clone()),
        metadata: None,
        archived_at: None,
    };
    temper
        .create_managed_agent(&agent_row)
        .await
        .context("seeding ManagedAgent")?;

    // ------------------------------------------------------------------
    // AgentTool: enable the built-in toolset on this agent.
    // ------------------------------------------------------------------
    let tool_row = AgentToolRow {
        id: format!("tool-{}", short_uuid()),
        agent_id: agent_id.clone(),
        kind: "agent_toolset".to_string(),
    };
    temper
        .create_agent_tool(&tool_row)
        .await
        .context("seeding AgentTool")?;

    // ------------------------------------------------------------------
    // Session: starts in Rescheduling per the IOA's initial state.
    // ------------------------------------------------------------------
    let session_row = SessionRow {
        id: session_id.clone(),
        agent_id: agent_id.clone(),
        environment_id: env_id.clone(),
        status: "Rescheduling".to_string(),
        created_at: opts.now.clone(),
        updated_at: opts.now.clone(),
        agent_version: Some(1),
        title: Some("crucible-chat".to_string()),
        metadata: None,
        active_seconds: Some(0.0),
        duration_seconds: Some(0.0),
        input_tokens: Some(0),
        output_tokens: Some(0),
        cache_read_input_tokens: Some(0),
        cache_creation_1h_input_tokens: Some(0),
        cache_creation_5m_input_tokens: Some(0),
        terminated_at: None,
        archived_at: None,
    };
    temper
        .create_session(&session_row)
        .await
        .context("seeding Session")?;

    Ok(SeedOutcome {
        environment_id: env_id,
        agent_id,
        session_id,
    })
}

fn short_uuid() -> String {
    Uuid::new_v4().simple().to_string()[..8].to_string()
}
