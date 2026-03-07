//! Temper Agent Runtime — core agent execution loop.
//!
//! Provides [`AgentRunner`] — a thin wrapper that creates an Agent entity and
//! runs a single LLM tool-call loop until completion. The agent decides its own
//! workflow using the Temper entity tools available to it.
//!
//! Pluggable via [`LlmProvider`] (LLM backend) and [`ToolRegistry`] (available tools).
//!
//! # Example
//!
//! ```no_run
//! use temper_agent_runtime::{AgentRunner, AnthropicProvider, LocalToolRegistry};
//! use temper_sdk::TemperClient;
//!
//! # async fn example() -> anyhow::Result<()> {
//! let client = TemperClient::new("http://127.0.0.1:4200", "default");
//! let provider = AnthropicProvider::new("claude-sonnet-4-6")?;
//! let tools = LocalToolRegistry::new(TemperClient::new("http://127.0.0.1:4200", "default"));
//! let pid = std::sync::Arc::new(std::sync::Mutex::new(None));
//! let runner = AgentRunner::new(client, Box::new(provider), Box::new(tools), pid);
//! let prompt = "You are a Temper agent. Use tools to accomplish goals.";
//! let id = runner.create_agent("developer", "Build a REST API", "claude-sonnet-4-6").await?;
//! let mut msgs = Vec::new();
//! let result = runner.send_autonomous(&id, prompt, "Build a REST API", &mut msgs, 200).await?;
//! runner.complete_agent(&id, &result).await?;
//! # Ok(())
//! # }
//! ```

pub mod providers;
pub mod runner;
pub mod sandbox;
pub mod tools;
pub use providers::LlmProvider;
pub use providers::anthropic::AnthropicProvider;
pub use providers::codex::CodexProvider;
pub use runner::AgentEvent;
pub use runner::AgentRunner;
pub use sandbox::AgentSandbox;
pub use sandbox::dispatch::{
    GovernanceCallback, GovernanceContext, GovernanceDecision, GovernanceEvent, GovernancePrompt,
    GovernanceResolverFn, GovernanceScope,
};
pub use tools::local::LocalToolRegistry;
pub use tools::sandbox::SandboxToolRegistry;
pub use tools::temper::TemperToolRegistry;
pub use tools::{ToolDef, ToolRegistry, ToolResult};
