//! Temper Agent Runtime — core agent execution loop.
//!
//! Provides [`AgentRunner`] for orchestrating the full agent lifecycle:
//! create agent, plan decomposition, task execution, and completion.
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
//! runner.run("Build a REST API", "developer").await?;
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
pub use runner::AgentRunner;
pub use sandbox::AgentSandbox;
pub use tools::local::LocalToolRegistry;
pub use tools::sandbox::SandboxToolRegistry;
pub use tools::temper::TemperToolRegistry;
pub use tools::{ToolDef, ToolRegistry, ToolResult};
