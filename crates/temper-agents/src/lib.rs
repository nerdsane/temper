//! Agent system actors built on temper-actor-runtime.
//!
//! Provides:
//! - IOA specs for Agent, ContextManager, ToolRouter, Compactor (embedded)
//! - Reaction rules wiring all actors (embedded)
//! - LlmIntegrationActor — AI Gateway via reqwest, streams via Bus actor
//! - ToolExecutorActor — dispatches tools via Bus actor (transport-agnostic)
//! - ToolRegistryActor — maps (session, tool_name) → client_id
//! - Mock integration actors for testing

pub mod child_completion;
pub mod child_spawner;
pub mod common;
pub mod llm;
pub mod tool_executor;
pub mod tool_registry;
pub mod wakeup;

use std::sync::Arc;

use temper_actor_runtime::spec_actor::SpecMessage;
use temper_actor_runtime::{Actor, ActorContext, ActorError, Message};
use temper_actor_runtime::{ActorSystem, SpecDrivenActor};
use temper_runtime::reaction::{ReactionRegistry, parse_reactions};

pub use llm::LlmIntegrationActor;
pub use tool_executor::{ToolDriver, ToolExecutorActor, ToolResult};
pub use tool_registry::ToolRegistryActor;

// ─── Embedded specs ──────────────────────────────────────────────────────────

pub const PROCESS_SPEC: &str = include_str!("../specs/process.ioa.toml");
pub const AGENT_DEFINITION_SPEC: &str = include_str!("../specs/agent_definition.ioa.toml");
pub const MESSAGE_SPEC: &str = include_str!("../specs/message.ioa.toml");
pub const AGENT_SPEC: &str = PROCESS_SPEC; // legacy alias
pub const CONTEXT_MANAGER_SPEC: &str = include_str!("../specs/context_manager.ioa.toml");
pub const TOOL_ROUTER_SPEC: &str = include_str!("../specs/tool_router.ioa.toml");
pub const COMPACTOR_SPEC: &str = include_str!("../specs/compactor.ioa.toml");
pub const PROCESS_REACTIONS: &str = include_str!("../specs/process.reactions.toml");
pub const AGENT_REACTIONS: &str = PROCESS_REACTIONS; // legacy alias

/// Names of all spec-driven actors in the agent system.
/// Session-scoped actor types (spawned per-conversation).
/// AgentDefinition and Message are NOT session actors — they're separate entities.
pub const AGENT_ACTOR_TYPES: &[&str] = &[
    "Process", // renamed from Agent
    "ContextManager",
    "ToolRouter",
    "Compactor",
    "LlmIntegration",
    "ContextAssemblerIntegration",
    "ToolExecutorIntegration",
    "CompactionIntegration",
    "ToolRegistry",
    // Primitive integrations
    "WakeupSchedulerIntegration",
    "ChildSpawnerIntegration",
    "ChildCompletionIntegration",
];

// ─── Agent system setup ───────────────────────────────────────────────────────

/// Register spec-driven actors (Process, AgentDefinition, Message, ContextManager,
/// ToolRouter, Compactor) and mock integration actors.
pub async fn register_agent_actors(
    system: &Arc<ActorSystem>,
) -> Result<(), Box<dyn std::error::Error>> {
    let rules = parse_reactions(PROCESS_REACTIONS)
        .map_err(|e| format!("failed to parse reactions: {e}"))?;
    let reactions = ReactionRegistry::from(rules);

    // Process (renamed from Agent) — the orchestrator state machine.
    system
        .register(Arc::new(SpecDrivenActor::from_ioa(
            PROCESS_SPEC,
            reactions.clone(),
        )?))
        .await?;
    // AgentDefinition — config template, spawns Process instances.
    system
        .register(Arc::new(SpecDrivenActor::from_ioa(
            AGENT_DEFINITION_SPEC,
            reactions.clone(),
        )?))
        .await?;
    // Message — conversation history, written by Process/LLM actors.
    system
        .register(Arc::new(SpecDrivenActor::from_ioa(
            MESSAGE_SPEC,
            reactions.clone(),
        )?))
        .await?;
    system
        .register(Arc::new(SpecDrivenActor::from_ioa(
            CONTEXT_MANAGER_SPEC,
            reactions.clone(),
        )?))
        .await?;
    system
        .register(Arc::new(SpecDrivenActor::from_ioa(
            TOOL_ROUTER_SPEC,
            reactions.clone(),
        )?))
        .await?;
    system
        .register(Arc::new(SpecDrivenActor::from_ioa(
            COMPACTOR_SPEC,
            reactions,
        )?))
        .await?;

    // Mock integrations — override with real actors for production use.
    system.register(Arc::new(MockContextAssembler)).await?;
    system.register(Arc::new(MockCompactionIntegration)).await?;

    // ToolRegistry — always real.
    system.register(Arc::new(ToolRegistryActor)).await?;

    Ok(())
}

/// Register real LLM + ToolExecutor + primitive integrations.
/// Call after register_agent_actors() and after registering the BusActor.
pub async fn register_real_integrations(
    system: &Arc<ActorSystem>,
    http: reqwest::Client,
    driver: Option<Arc<dyn ToolDriver>>,
) -> Result<(), Box<dyn std::error::Error>> {
    system
        .register(Arc::new(LlmIntegrationActor::new(http.clone())))
        .await?;
    let executor = match driver {
        Some(d) => ToolExecutorActor::with_driver(d),
        None => ToolExecutorActor::new(),
    };
    system.register(Arc::new(executor)).await?;

    // Primitive integrations (spawn, sleep, child completion).
    let system_arc = system.clone();
    system
        .register(Arc::new(wakeup::WakeupSchedulerIntegration))
        .await?;
    system
        .register(Arc::new(child_spawner::ChildSpawnerIntegration {
            actor_system: system_arc.clone(),
        }))
        .await?;
    system
        .register(Arc::new(child_completion::ChildCompletionIntegration {
            actor_system: system_arc,
        }))
        .await?;
    Ok(())
}

// ─── Mock integration actors (for testing) ───────────────────────────────────

fn message_action(message: &Message) -> String {
    common::message_action(message)
}

/// Mock context assembler. Returns ContextReady immediately.
pub struct MockContextAssembler;

#[async_trait::async_trait]
impl Actor for MockContextAssembler {
    fn actor_type(&self) -> &str {
        "ContextAssemblerIntegration"
    }

    async fn handle(
        &self,
        ctx: &ActorContext,
        _state: &mut Vec<u8>,
        message: &Message,
    ) -> Result<(), ActorError> {
        if message_action(message) == "assemble_context"
            && let Some(from) = &message.from
        {
            ctx.tell(from, SpecMessage::new("ContextReady")).await?;
        }
        Ok(())
    }
}

/// Mock compaction integration. Returns CompactionComplete immediately.
pub struct MockCompactionIntegration;

#[async_trait::async_trait]
impl Actor for MockCompactionIntegration {
    fn actor_type(&self) -> &str {
        "CompactionIntegration"
    }

    async fn handle(
        &self,
        ctx: &ActorContext,
        _state: &mut Vec<u8>,
        message: &Message,
    ) -> Result<(), ActorError> {
        if message_action(message) == "run_compaction"
            && let Some(from) = &message.from
        {
            ctx.tell(from, SpecMessage::new("CompactionComplete"))
                .await?;
        }
        Ok(())
    }
}

/// Mock LLM integration for testing (no AI Gateway, no Bus).
pub struct MockLlmIntegration;

#[async_trait::async_trait]
impl Actor for MockLlmIntegration {
    fn actor_type(&self) -> &str {
        "LlmIntegration"
    }

    async fn handle(
        &self,
        ctx: &ActorContext,
        _state: &mut Vec<u8>,
        message: &Message,
    ) -> Result<(), ActorError> {
        if message_action(message) == "invoke_llm"
            && let Some(from) = &message.from
        {
            ctx.tell(from, SpecMessage::new("InferenceCompleteEndTurn"))
                .await?;
        }
        Ok(())
    }
}

/// Mock tool executor for testing (no Bus, no Zenoh).
pub struct MockToolExecutor;

#[async_trait::async_trait]
impl Actor for MockToolExecutor {
    fn actor_type(&self) -> &str {
        "ToolExecutorIntegration"
    }

    async fn handle(
        &self,
        ctx: &ActorContext,
        _state: &mut Vec<u8>,
        message: &Message,
    ) -> Result<(), ActorError> {
        if message_action(message) == "execute_tool_batch"
            && let Some(from) = &message.from
        {
            ctx.tell(from, SpecMessage::new("ToolCallBatchComplete"))
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_reactions_route_core_effects() {
        let rules = parse_reactions(PROCESS_REACTIONS).expect("parse reactions");
        let reactions = ReactionRegistry::from(rules);
        assert_eq!(
            reactions.lookup("Process", "PrepareContext", "")[0]
                .then
                .entity_type,
            "ContextManager"
        );
        assert_eq!(
            reactions.lookup("Process", "ToolCallBatchRequested", "")[0]
                .then
                .entity_type,
            "ToolRouter"
        );
        assert_eq!(
            reactions.lookup("Process", "ToolCallBatchRequested", "")[0]
                .then
                .params_from
                .get("tool_calls")
                .map(String::as_str),
            Some("tool_calls")
        );
        assert_eq!(
            reactions.lookup("Process", "spawn_child", "")[0]
                .then
                .entity_type,
            "ChildSpawnerIntegration"
        );
        assert_eq!(
            reactions.lookup("Process", "schedule_wakeup", "")[0]
                .then
                .entity_type,
            "WakeupSchedulerIntegration"
        );
        assert_eq!(
            reactions.lookup("Process", "notify_parent", "Terminated")[0]
                .then
                .entity_type,
            "ChildCompletionIntegration"
        );
        assert_eq!(
            reactions.lookup("Process", "notify_parent", "Terminated")[0]
                .then
                .params_from
                .get("child_result")
                .map(String::as_str),
            Some("final_answer")
        );
    }
}
