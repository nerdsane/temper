//! AI agent modules for the platform.
//!
//! - [`claude`]: Claude API client for LLM-powered conversations
//! - [`developer`]: Developer interview agent that orchestrates entity discovery
//! - [`production`]: Production chat agent for end users (placeholder)

pub mod claude;
pub mod developer;
pub mod production;

pub use claude::ClaudeClient;
pub use developer::DeveloperAgent;
pub use production::ProductionAgent;
