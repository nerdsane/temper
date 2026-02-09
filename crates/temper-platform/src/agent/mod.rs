//! AI agent modules for the platform.
//!
//! - [`claude`]: Claude API client for LLM-powered evolution agents

pub mod claude;

pub use claude::ClaudeClient;
